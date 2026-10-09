//! MCP frontends. Every frontend is stateless: the broker owns executions, the
//! journal and Events subscriptions, so any number of stdio (e.g. tunnel-client)
//! and Streamable HTTP clients can be connected at the same time.
use crate::{broker::Client, events::Events, workspace};
use anyhow::Result;
use rmcp::{
    ErrorData as McpError, RoleServer, ServerHandler, Service, ServiceExt,
    model::*,
    service::{NotificationContext, RequestContext},
};
use serde_json::{Value, json};
use std::{borrow::Cow, sync::Arc};

/// 2026-07-28 uses `server/discover`; earlier revisions use `initialize`.
const VERSIONS: [ProtocolVersion; 4] = [
    ProtocolVersion::V_2026_07_28,
    ProtocolVersion::V_2025_11_25,
    ProtocolVersion::V_2025_06_18,
    ProtocolVersion::V_2025_03_26,
];
const INSTRUCTIONS: &str = "Open a project with open_session, then start explicit argv executions. profile=host has the OS user's full file and network rights; profile=sandbox limits writes to the session roots and disables network. Follow output with wait_execution (view=summary is compact) and pass back the returned top-level cursor; read_execution recovers after reconnects or gaps. Never blindly resend a start or stdin whose result was uncertain: recover through list_sessions/open_session by execution_id or idempotency_key. Events (execution.events, webhook) are optional push delivery.";

#[derive(Clone)]
pub struct Server {
    broker: Client,
    tools: Arc<Vec<Tool>>,
}
impl Server {
    pub async fn new() -> Result<Self> {
        Ok(Self {
            broker: Client::new().await?,
            tools: Arc::new(serde_json::from_value(json!(definitions()))?),
        })
    }
}
fn definitions() -> Vec<Value> {
    let id = json!({"type":"string","maxLength":128});
    let text = json!({"type":"string","maxLength":65536});
    let path = json!({"type":"string","maxLength":4096});
    let view = json!({"type":"string","enum":["summary","detail"],"default":"detail"});
    let mut tools = Vec::new();
    let mut add = |name: &str,
                   description: &str,
                   properties: Value,
                   required: &[&str],
                   read_only: bool| {
        tools.push(json!({"name":name,"description":description,"inputSchema":{"type":"object","properties":properties,"required":required,"additionalProperties":false},"annotations":{"readOnlyHint":read_only,"destructiveHint":!read_only,"openWorldHint":true}}));
    };
    add(
        "open_session",
        "Open a project directory. The same canonical directory always returns the same session. No shell is started.",
        json!({"cwd":path}),
        &["cwd"],
        false,
    );
    add(
        "list_sessions",
        "List project sessions, execution records, checkpoints and available execution profiles.",
        json!({}),
        &[],
        true,
    );
    add(
        "close_session",
        "Kill the session's running executions, confirm their exit and remove the session. If pending=true, call again later. Project files are kept.",
        json!({"session_id":id}),
        &["session_id"],
        false,
    );
    add(
        "start_execution",
        "Start an argv once and return its execution record. profile: host (full OS-user rights, no approval), sandbox (writes only in session roots, no network, io=pipes) or admin:<id> (operator-defined bubblewrap, pipes only; see list_sessions.execution_profiles). io=pty merges stdout/stderr; pipes separates them. For safe retry after a lost response, pass idempotency_key with the key_generation from open_session/list_sessions and reuse the identical request.",
        json!({"session_id":id,"command":{"type":"array","items":text,"minItems":1,"maxItems":256},"profile":{"type":"string","pattern":"^(sandbox|host|admin:[A-Za-z0-9_-]{1,64})$","maxLength":70},"io":{"type":"string","enum":["pty","pipes"],"default":"pty"},"cwd":path,"idempotency_key":id,"key_generation":id,"work_id":id,"purpose":{"type":"string","maxLength":2048},"completion_condition":{"type":"string","maxLength":2048}}),
        &["session_id", "command", "profile"],
        false,
    );
    add(
        "input_execution",
        "Write literal UTF-8 to stdin. close_stdin=true sends EOF on pipes after the text (for a PTY send \\u0004 instead). The result reports acceptance, not that the program read it; never resend uncertain input automatically.",
        json!({"execution_id":id,"text":text,"close_stdin":{"type":"boolean","default":false}}),
        &["execution_id"],
        false,
    );
    add(
        "resize_execution",
        "Resize a running PTY.",
        json!({"execution_id":id,"rows":{"type":"integer","minimum":1,"maximum":1000},"cols":{"type":"integer","minimum":1,"maximum":1000}}),
        &["execution_id", "rows", "cols"],
        false,
    );
    add(
        "signal_execution",
        "Send INT, TERM or KILL to the execution's process group.",
        json!({"execution_id":id,"signal":{"type":"string","enum":["INT","TERM","KILL"]}}),
        &["execution_id", "signal"],
        false,
    );
    add(
        "read_execution",
        "Read execution state (view=summary) or sequenced output after cursor (view=detail). History is bounded: catch_up_required marks loss and more=true means another page remains.",
        json!({"execution_id":id,"cursor":id,"view":view}),
        &["execution_id"],
        true,
    );
    add(
        "wait_execution",
        "Wait up to max_wait_ms (0-10000, default 1000) for output, state change or exit after cursor, then return like read_execution plus timed_out. Pass back the top-level cursor; a timeout does not mean the program is waiting for input.",
        json!({"execution_id":id,"cursor":id,"state_cursor":id,"view":view,"max_wait_ms":{"type":"integer","minimum":0,"maximum":10000,"default":1000}}),
        &["execution_id", "cursor"],
        true,
    );
    add(
        "checkpoint_execution",
        "Persist a reader's processed cursor and work purpose with compare-and-set (expected_revision, 0 creates). Set completed=true only after verifying the work is done and the process exited.",
        json!({"execution_id":id,"reader_id":id,"cursor":id,"completed":{"type":"boolean"},"expected_revision":{"type":"integer","minimum":0},"purpose":{"type":"string","maxLength":2048},"completion_condition":{"type":"string","maxLength":2048}}),
        &["execution_id", "reader_id", "cursor", "expected_revision"],
        false,
    );
    add(
        "read_file",
        "Read up to 64 KiB of UTF-8 text with the OS user's access. Relative paths use the session cwd.",
        json!({"session_id":id,"path":path}),
        &["session_id", "path"],
        true,
    );
    add(
        "write_file",
        "Write up to 64 KiB of UTF-8 from inside the sandbox; the parent must lie within the session roots.",
        json!({"session_id":id,"path":path,"content":text}),
        &["session_id", "path", "content"],
        false,
    );
    add(
        "list_directory",
        "List up to 256 directory entries with the OS user's access.",
        json!({"session_id":id,"path":path}),
        &["session_id", "path"],
        true,
    );
    add(
        "get_image",
        "Return a PNG/JPEG/GIF/WebP file up to 1 MiB as image content.",
        json!({"session_id":id,"path":path}),
        &["session_id", "path"],
        true,
    );
    tools.push(crate::files::tool());
    tools
}

fn resource_args(uri: &str) -> anyhow::Result<Value> {
    use anyhow::ensure;
    ensure!(uri.len() <= 1024, "resource URI too long");
    let url = url::Url::parse(uri)?;
    ensure!(
        url.scheme() == "dev-session" && url.host_str().is_none() && url.fragment().is_none(),
        "invalid recovery URI"
    );
    if url.path() == "/recovery" {
        ensure!(url.query().is_none(), "unexpected query");
        return Ok(json!({"op":"list_sessions"}));
    }
    let mut args = if let Some(id) = url.path().strip_prefix("/executions/") {
        json!({"op":"read_execution","execution_id":id})
    } else if let Some(id) = url.path().strip_prefix("/source/") {
        ensure!(url.query().is_none(), "unexpected source query");
        json!({"op":"execution_source","execution_id":id})
    } else {
        anyhow::bail!("unknown recovery resource")
    };
    let encoded = format!("id={}", args["execution_id"].as_str().unwrap());
    let decoded = url::form_urlencoded::parse(encoded.as_bytes())
        .next()
        .unwrap()
        .1
        .into_owned();
    ensure!(
        !decoded.is_empty()
            && decoded.len() <= 128
            && decoded
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b':' | b'_' | b'-')),
        "invalid execution resource ID"
    );
    args["execution_id"] = json!(decoded);
    let mut seen = std::collections::HashSet::new();
    for (key, value) in url.query_pairs() {
        ensure!(
            matches!(key.as_ref(), "cursor" | "view") && seen.insert(key.to_string()),
            "invalid resource query"
        );
        args[key.as_ref()] = json!(value);
    }
    Ok(args)
}

fn tool_error(message: &str) -> CallToolResponse {
    CallToolResult::error(vec![ContentBlock::text(
        workspace::bounded(message, 1024, false).0,
    )])
    .into()
}
impl ServerHandler for Server {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_resources()
                .build(),
        )
        .with_server_info(Implementation::new(
            env!("CARGO_PKG_NAME"),
            env!("CARGO_PKG_VERSION"),
        ))
        .with_instructions(INSTRUCTIONS)
    }
    fn get_tool(&self, name: &str) -> Option<Tool> {
        self.tools.iter().find(|t| t.name == name).cloned()
    }
    fn supported_protocol_versions(&self) -> Cow<'static, [ProtocolVersion]> {
        Cow::Borrowed(&VERSIONS)
    }
    async fn list_tools(
        &self,
        _: Option<PaginatedRequestParams>,
        _: RequestContext<RoleServer>,
    ) -> std::result::Result<ListToolsResult, McpError> {
        let mut result = ListToolsResult::default()
            .with_ttl_ms(0)
            .with_cache_scope(CacheScope::Private);
        result.tools = self.tools.to_vec();
        Ok(result)
    }
    async fn list_resources(
        &self,
        _: Option<PaginatedRequestParams>,
        _: RequestContext<RoleServer>,
    ) -> std::result::Result<ListResourcesResult, McpError> {
        let mut result = ListResourcesResult::default()
            .with_ttl_ms(0)
            .with_cache_scope(CacheScope::Private);
        result.resources = vec![
            Resource::new("dev-session:///recovery", "Execution recovery index")
                .with_mime_type("application/json"),
        ];
        Ok(result)
    }
    async fn list_resource_templates(
        &self,
        _: Option<PaginatedRequestParams>,
        _: RequestContext<RoleServer>,
    ) -> std::result::Result<ListResourceTemplatesResult, McpError> {
        serde_json::from_value(json!({"resultType":"complete","ttlMs":0,"cacheScope":"private","resourceTemplates":[
            {"uriTemplate":"dev-session:///executions/{execution_id}{?cursor,view}","name":"Execution state or bounded detail","mimeType":"application/json"},
            {"uriTemplate":"dev-session:///source/{execution_id}","name":"Original execution argv","mimeType":"application/json"}
        ]})).map_err(|_| McpError::internal_error("invalid resource templates",None))
    }
    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _: RequestContext<RoleServer>,
    ) -> std::result::Result<ReadResourceResponse, McpError> {
        let args = resource_args(&request.uri)
            .map_err(|e| McpError::invalid_params(e.to_string(), None))?;
        let data = self.broker.call(args).await.map_err(|e| {
            McpError::invalid_params(workspace::bounded(&e.to_string(), 1024, false).0, None)
        })?;
        Ok(ReadResourceResult::new(vec![ResourceContents::text(
            serde_json::to_string(&data).unwrap(),
            request.uri,
        )])
        .with_ttl_ms(0)
        .with_cache_scope(CacheScope::Private)
        .into())
    }
    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> std::result::Result<CallToolResponse, McpError> {
        if self.get_tool(&request.name).is_none() {
            return Err(McpError::invalid_params("unknown tool", None));
        }
        let mut args = json!(request.arguments.unwrap_or_default());
        let response = if request.name == "wait_execution" {
            tokio::select! {
                biased;
                _ = context.ct.cancelled() => Err(anyhow::anyhow!("wait_execution cancelled")),
                result = crate::wait::execution(&self.broker, args) => result,
            }
        } else {
            args["op"] = json!(request.name);
            self.broker.call(args).await
        };
        Ok(match response {
            Ok(data) if request.name == "get_image" => {
                serde_json::from_value::<CallToolResult>(data)
                    .map_err(|_| McpError::internal_error("invalid image response", None))?
                    .into()
            }
            Ok(data) => {
                let mut result = CallToolResult::success(vec![ContentBlock::text(
                    serde_json::to_string(&data).unwrap(),
                )]);
                result.structured_content = Some(data);
                result.into()
            }
            Err(error) => tool_error(&error.to_string()),
        })
    }
    async fn on_custom_request(
        &self,
        request: CustomRequest,
        _: RequestContext<RoleServer>,
    ) -> std::result::Result<CustomResult, McpError> {
        let params = request.params.unwrap_or(json!({}));
        let op = match request.method.as_str() {
            "events/list" => return Ok(CustomResult(Events::list())),
            "events/subscribe" => "events_subscribe",
            "events/unsubscribe" => "events_unsubscribe",
            _ => {
                return Err(McpError::new(
                    ErrorCode::METHOD_NOT_FOUND,
                    "unknown method",
                    None,
                ));
            }
        };
        let result = self.broker.call(json!({"op":op,"params":params})).await;
        result.map(CustomResult).map_err(|error| {
            let reason = error.to_string();
            if [
                "invalid_callback",
                "private_callback",
                "dns_failed",
                "timeout_or_transport",
                "challenge_failed",
                "response_failed",
                "response_too_large",
            ]
            .contains(&reason.as_str())
            {
                McpError::new(
                    ErrorCode(-32015),
                    "callback verification failed",
                    Some(json!({"reason":reason})),
                )
            } else {
                McpError::invalid_params(workspace::bounded(&reason, 1024, false).0, None)
            }
        })
    }
}

/// The Events extension declares a top-level `capabilities.events` in
/// `server/discover`; rmcp's typed capabilities have no such field.
pub fn advertise_events(discover: &mut Value) {
    if discover["capabilities"].is_object() {
        discover["capabilities"]["events"] = json!({});
    }
}
struct EventService(Server);
impl Service<RoleServer> for EventService {
    async fn handle_request(
        &self,
        request: ClientRequest,
        context: RequestContext<RoleServer>,
    ) -> std::result::Result<ServerResult, McpError> {
        let discovery = matches!(request, ClientRequest::DiscoverRequest(_));
        let result = Service::handle_request(&self.0, request, context).await?;
        if !discovery {
            return Ok(result);
        }
        let mut value = serde_json::to_value(result)
            .map_err(|_| McpError::internal_error("discovery serialization failed", None))?;
        advertise_events(&mut value);
        Ok(ServerResult::CustomResult(CustomResult(value)))
    }
    async fn handle_notification(
        &self,
        notification: ClientNotification,
        context: NotificationContext<RoleServer>,
    ) -> std::result::Result<(), McpError> {
        Service::handle_notification(&self.0, notification, context).await
    }
    fn supported_protocol_versions(&self) -> Cow<'static, [ProtocolVersion]> {
        ServerHandler::supported_protocol_versions(&self.0)
    }
    fn get_info(&self) -> ServerConfig {
        ServerHandler::get_info(&self.0)
    }
}
pub async fn stdio() -> Result<()> {
    EventService(Server::new().await?)
        .serve(rmcp::transport::stdio())
        .await?
        .waiting()
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_and_discovery_capability() {
        let tools: Vec<Tool> = serde_json::from_value(json!(definitions())).unwrap();
        assert_eq!(tools.len(), 15);
        let mut discover = json!({"capabilities":{"tools":{}}});
        advertise_events(&mut discover);
        assert_eq!(discover["capabilities"]["events"], json!({}));
        let mut error = json!({"error":{"code":-1}});
        advertise_events(&mut error);
        assert!(error.get("capabilities").is_none());
    }
}
