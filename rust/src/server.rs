use crate::{broker::Client, events::Events, workspace};
use anyhow::Result;
use rmcp::{
    ErrorData as McpError, RoleServer, ServerHandler, Service, ServiceExt,
    model::*,
    service::{NotificationContext, RequestContext},
};
use serde_json::{Value, json};

#[derive(Clone)]
struct Server {
    broker: Client,
    events: Events,
    tools: Vec<Tool>,
}
impl Server {
    async fn new() -> Result<Self> {
        let broker = Client::new().await?;
        let events = Events::new(broker.clone()).await?;
        let tools = definitions();
        Ok(Self {
            broker,
            events,
            tools: serde_json::from_value(json!(tools))?,
        })
    }
}
fn definitions() -> Vec<Value> {
    let id = json!({"type":"string","maxLength":128});
    let text = json!({"type":"string","maxLength":65536});
    let path = json!({"type":"string","maxLength":4096});
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
        "Open a project directory. Reopening the same canonical directory returns the same session. No implicit shell, locks, or worktrees.",
        json!({"cwd":path}),
        &["cwd"],
        false,
    );
    add(
        "list_sessions",
        "List project sessions and bounded execution records; reconnect by their explicit IDs.",
        json!({}),
        &[],
        true,
    );
    add(
        "close_session",
        "Mark closing, signal managed executions independently of stdin, and confirm their exit before metadata removal. If pending=true, retry close after exit. Bounded terminal history and existing Events leases remain readable; project files are preserved.",
        json!({"session_id":id}),
        &["session_id"],
        false,
    );
    add(
        "start_execution",
        "Start argv once. Persist idempotency_key and recovery.key_generation from list/open before sending; reuse only that pair for an identical request to recover a lost ACK. Unknown outcomes never auto-restart. Optional work_id/purpose/completion_condition survive reconnect. host uses FULL OS-user filesystem/network rights without approval; sandbox restricts writes to session roots and disables network, and requires io=pipes. PTY merges stdout/stderr. Subscribe to execution.events for batched push; read_execution is for reconnection/gaps. Returns the execution record. No automatic shell or implicit selected job.",
        json!({"session_id":id,"command":{"type":"array","items":text,"minItems":1,"maxItems":256},"profile":{"type":"string","enum":["sandbox","host"]},"io":{"type":"string","enum":["pty","pipes"],"default":"pty"},"cwd":path,"idempotency_key":id,"key_generation":id,"work_id":id,"purpose":{"type":"string","maxLength":2048},"completion_condition":{"type":"string","maxLength":2048}}),
        &["session_id", "command", "profile"],
        false,
    );
    add(
        "input_execution",
        "Queue literal UTF-8 stdin. close_stdin=true closes only a pipe after the queued text, producing EOF; PTY Ctrl-D is literal text, not pipe close. Acceptance differs from OS write; Events report written/delivery_unknown and stdin_closed. Never automatically resend uncertain input.",
        json!({"execution_id":id,"text":text,"close_stdin":{"type":"boolean","default":false}}),
        &["execution_id"],
        false,
    );
    add(
        "resize_execution",
        "Resize a running host PTY without restarting it. Returns the same execution record; Events report application.",
        json!({"execution_id":id,"rows":{"type":"integer","minimum":1,"maximum":1000},"cols":{"type":"integer","minimum":1,"maximum":1000}}),
        &["execution_id", "rows", "cols"],
        false,
    );
    add(
        "signal_execution",
        "Send INT, TERM or KILL to this owned execution's process group. Bare/stale PIDs are rejected. Completion is delivered by Events; detached descendants require explicit project management.",
        json!({"execution_id":id,"signal":{"type":"string","enum":["INT","TERM","KILL"]}}),
        &["execution_id", "signal"],
        false,
    );
    add(
        "read_execution",
        "Use view=summary for compact state without consuming detail; view=detail (legacy default) reads sequenced output. Recover execution state after reconnect or an event gap. Pass the last cursor; deduplicate by sequence. History is bounded, catch_up_required marks loss, more means another bounded page remains. This is a recovery API; ordinary updates arrive through Events.",
        json!({"execution_id":id,"cursor":id,"view":{"type":"string","enum":["summary","detail"],"default":"detail"}}),
        &["execution_id"],
        true,
    );
    add(
        "wait_execution",
        "Use view=summary for compact updates; cursor is processed detail position, state_cursor optionally resumes observation independently. Default detail preserves legacy output. Bounded ordinary read-only tool, not Events streaming. Return output/state/exit after the required cursor, or immediately if terminal or a gap exists. max_wait_ms defaults to 1000 (0..10000); detail snapshots add at most 2000 ms transport budget; summary adds one snapshot and a capability probe (4000 ms total transport budget). Timeout returns state and cursor with timed_out=true. Preserve global epoch:sequence cursors, deduplicate by sequence, and drain more pages explicitly. No automatic repeat, execution or stdin resend.",
        json!({"execution_id":id,"cursor":id,"state_cursor":id,"view":{"type":"string","enum":["summary","detail"],"default":"detail"},"max_wait_ms":{"type":"integer","minimum":0,"maximum":10000,"default":1000}}),
        &["execution_id", "cursor"],
        true,
    );
    add(
        "read_delivery_diagnostics",
        "Read delivery lease, suspension, pending-batch and bounded HTTP timing history, separately from optional execution state. No callback URL, credentials or output bodies. Does not retry or refresh. HTTP receipt does not prove chat forwarding; absent subscriptions mean no retained evidence.",
        json!({"session_id":id,"execution_id":id}),
        &["session_id"],
        true,
    );
    add(
        "read_file",
        "Read up to 64 KiB UTF-8 text with a structured truncation flag. Host OS-user file access; relative path uses session cwd.",
        json!({"session_id":id,"path":path}),
        &["session_id", "path"],
        true,
    );
    add(
        "write_file",
        "Write up to 64 KiB UTF-8 in the Codex sandbox, with network disabled. Destination parent must lie within session permitted roots. No self-declared approval or permission expansion.",
        json!({"session_id":id,"path":path,"content":text}),
        &["session_id", "path", "content"],
        false,
    );
    add(
        "list_directory",
        "List bounded host directory entries. Host OS-user file access; relative path uses session cwd.",
        json!({"session_id":id,"path":path}),
        &["session_id", "path"],
        true,
    );
    add(
        "get_image",
        "Read a PNG/JPEG/GIF/WebP up to 1 MiB as image content. Host OS-user file access.",
        json!({"session_id":id,"path":path}),
        &["session_id", "path"],
        true,
    );
    add(
        "checkpoint_execution",
        "Persist this reader's processed detail cursor and work purpose/completion condition using expected_revision (0 creates). CAS conflicts require reading list_sessions/open_session or the recovery resource. Reads never acknowledge output. Set completed=true only after verifying work completion and confirmed process exit; this allows eventual history retirement. Does not execute commands or resend stdin.",
        json!({"execution_id":id,"reader_id":id,"cursor":id,"completed":{"type":"boolean"},"expected_revision":{"type":"integer","minimum":0},"purpose":{"type":"string","maxLength":2048},"completion_condition":{"type":"string","maxLength":2048}}),
        &["execution_id", "reader_id", "cursor", "expected_revision"],
        false,
    );
    tools.push(crate::files::tool());
    tools
}
async fn require_recovery(broker: &Client) -> anyhow::Result<()> {
    let info = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        broker.call(json!({"op":"ping"})),
    )
    .await
    .map_err(|_| anyhow::anyhow!("broker capability check timed out; no command sent"))??;
    anyhow::ensure!(
        info["recovery"] == 2,
        "broker lacks recovery v2; no command sent. Legacy detail tools remain available"
    );
    Ok(())
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
impl ServerHandler for Server {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().enable_resources().build()).with_server_info(Implementation::new(env!("CARGO_PKG_NAME"),env!("CARGO_PKG_VERSION"))).with_instructions("Explicit project sessions and executions. Prefer read_execution/wait_execution view=summary; fetch detail only when needed. Keep state_cursor separate from processed detail cursor. Persist reader progress with checkpoint_execution CAS; explicitly complete verified work so history can retire. Persist the start key with its key_generation; never refresh an expired generation to retry an old request. On uncertain start/input results, never blindly resend; recover by IDs or idempotency_key through list_sessions/open_session. Existing execution.events subscriptions are optional. Alternatively use wait_execution for one bounded ordinary tool response; it does not automatically continue work. Host execution has full OS-user rights; sandbox execution disables network. Input acceptance is not an application acknowledgement. No implicit shell, default execution, or legacy API.")
    }
    fn get_tool(&self, name: &str) -> Option<Tool> {
        self.tools.iter().find(|t| t.name == name).cloned()
    }
    fn supported_protocol_versions(&self) -> std::borrow::Cow<'static, [ProtocolVersion]> {
        std::borrow::Cow::Owned(vec![ProtocolVersion::V_2026_07_28])
    }
    async fn initialize(
        &self,
        _: InitializeRequestParams,
        _: RequestContext<RoleServer>,
    ) -> std::result::Result<InitializeResult, McpError> {
        Err(McpError::new(
            ErrorCode::METHOD_NOT_FOUND,
            "use server/discover with per-request metadata",
            None,
        ))
    }
    async fn list_tools(
        &self,
        _: Option<PaginatedRequestParams>,
        _: RequestContext<RoleServer>,
    ) -> std::result::Result<ListToolsResult, McpError> {
        let mut result = ListToolsResult::default()
            .with_ttl_ms(0)
            .with_cache_scope(CacheScope::Private);
        result.tools = self.tools.clone();
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
        require_recovery(&self.broker)
            .await
            .map_err(|e| McpError::invalid_params(e.to_string(), None))?;
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
        let needs_recovery = request.name == "checkpoint_execution"
            || [
                "idempotency_key",
                "key_generation",
                "work_id",
                "purpose",
                "completion_condition",
                "state_cursor",
            ]
            .iter()
            .any(|key| args.get(key).is_some())
            || args["view"] == "summary";
        if needs_recovery {
            if let Err(error) = require_recovery(&self.broker).await {
                return Ok(
                    CallToolResult::error(vec![ContentBlock::text(error.to_string())]).into(),
                );
            }
        }
        let response = if request.name == "wait_execution" {
            tokio::select! {
                biased;
                _ = context.ct.cancelled() => Err(anyhow::anyhow!("wait_execution cancelled")),
                result = crate::wait::execution(&self.broker, args) => result,
            }
        } else if request.name == "read_delivery_diagnostics" {
            self.events.diagnostics(args).await
        } else {
            args["op"] = json!(request.name);
            self.broker.call(args).await
        };
        let result = match response {
            Ok(data) if request.name == "get_image" => serde_json::from_value(data)
                .map_err(|_| McpError::internal_error("invalid image response", None))?,
            Ok(data) => {
                let mut result = CallToolResult::success(vec![ContentBlock::text(
                    serde_json::to_string(&data).unwrap(),
                )]);
                result.structured_content = Some(data);
                result
            }
            Err(error) => CallToolResult::error(vec![ContentBlock::text(
                workspace::bounded(&error.to_string(), 1024, false).0,
            )]),
        };
        Ok(result.into())
    }
    async fn on_custom_request(
        &self,
        request: CustomRequest,
        _: RequestContext<RoleServer>,
    ) -> std::result::Result<CustomResult, McpError> {
        let params = request.params.unwrap_or(json!({}));
        let result = match request.method.as_str() {
            "events/list" => Ok(Events::list()),
            "events/subscribe" => self.events.subscribe(params).await,
            "events/unsubscribe" => self.events.unsubscribe(params).await,
            _ => {
                return Err(McpError::new(
                    ErrorCode::METHOD_NOT_FOUND,
                    "unknown method",
                    None,
                ));
            }
        };
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
struct EventService(Server);
impl Service<RoleServer> for EventService {
    async fn handle_request(
        &self,
        request: ClientRequest,
        context: RequestContext<RoleServer>,
    ) -> std::result::Result<ServerResult, McpError> {
        let discovery = matches!(request, ClientRequest::DiscoverRequest(_));
        let result = Service::handle_request(&self.0, request, context).await?;
        if discovery {
            // The SDK validates the inline lifecycle, but its typed capabilities
            // omit draft Events. Preserve validation and extend only wire discovery.
            let mut value = serde_json::to_value(result)
                .map_err(|_| McpError::internal_error("discovery serialization failed", None))?;
            value["capabilities"]["events"] = json!({});
            Ok(ServerResult::CustomResult(CustomResult(value)))
        } else {
            Ok(result)
        }
    }
    async fn handle_notification(
        &self,
        notification: ClientNotification,
        context: NotificationContext<RoleServer>,
    ) -> std::result::Result<(), McpError> {
        Service::handle_notification(&self.0, notification, context).await
    }
    fn supported_protocol_versions(&self) -> std::borrow::Cow<'static, [ProtocolVersion]> {
        ServerHandler::supported_protocol_versions(&self.0)
    }
    fn get_info(&self) -> ServerConfig {
        ServerHandler::get_info(&self.0)
    }
}
pub async fn stdio() -> Result<()> {
    workspace::clean_environment()?;
    let state = workspace::state_dir()?;
    std::fs::create_dir_all(&state)?;
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(state.join("frontend.lock"))?;
    use std::os::fd::AsRawFd;
    anyhow::ensure!(
        unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0,
        "another stdio frontend owns this state"
    );
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

    #[tokio::test]
    async fn diagnostics_mcp_protocol_boundary_in_memory() -> Result<()> {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        let root = tempfile::tempdir()?;
        // A deliberately absent broker: valid queries must return a sanitized
        // tool error. This is NOT a real socket/stdio success test.
        let broker = Client::test_socket(root.path().join("absent.sock"));
        let events = Events::empty_for_protocol_test(
            broker.clone(),
            root.path().join("must-not-be-created.json"),
        );
        let server = EventService(Server {
            broker,
            events,
            tools: serde_json::from_value(json!(definitions()))?,
        });
        let (server_io, client_io) = tokio::io::duplex(65536);
        let task = tokio::spawn(async move {
            server.serve(server_io).await?.waiting().await?;
            Ok::<_, anyhow::Error>(())
        });
        let mut client = BufReader::new(client_io);
        let meta = json!({
            "io.modelcontextprotocol/protocolVersion":"2026-07-28",
            "io.modelcontextprotocol/clientInfo":{"name":"diagnostic-boundary","version":"1"},
            "io.modelcontextprotocol/clientCapabilities":{}
        });
        let cases = [
            ("server/discover", json!({}), None),
            ("tools/list", json!({}), None),
            (
                "tools/call",
                json!({"name":"read_delivery_diagnostics","arguments":{}}),
                Some("missing string session_id"),
            ),
            (
                "tools/call",
                json!({"name":"read_delivery_diagnostics","arguments":{"session_id":"s","execution_id":null}}),
                Some("missing string execution_id"),
            ),
            (
                "tools/call",
                json!({"name":"read_delivery_diagnostics","arguments":{"session_id":"s","owner":0,"secret":"PRIVATE_CANARY"}}),
                Some("unknown diagnostic argument"),
            ),
            (
                "tools/call",
                json!({"name":"read_delivery_diagnostics","arguments":{"session_id":"s"}}),
                Some("session diagnostics unavailable"),
            ),
            (
                "tools/call",
                json!({"name":"nonexistent_tool","arguments":{}}),
                None,
            ),
            ("events/diagnostics", json!({}), None),
        ];
        for (i, (method, mut params, expected)) in cases.into_iter().enumerate() {
            params["_meta"] = meta.clone();
            let request = json!({"jsonrpc":"2.0","id":i,"method":method,"params":params});
            client
                .get_mut()
                .write_all(format!("{request}\n").as_bytes())
                .await?;
            let mut line = String::new();
            tokio::time::timeout(
                std::time::Duration::from_secs(5),
                client.read_line(&mut line),
            )
            .await??;
            let response: Value = serde_json::from_str(&line)?;
            assert_eq!(response["id"], i);
            assert!(!line.contains("PRIVATE_CANARY"));
            assert!(!line.contains("absent.sock"));
            if let Some(message) = expected {
                assert!(
                    response.get("error").is_none(),
                    "tool failures belong in CallToolResult"
                );
                assert_eq!(response["result"]["isError"], true);
                assert_eq!(response["result"]["content"][0]["text"], message);
            } else {
                match i {
                    0 => assert_eq!(response["result"]["capabilities"]["events"], json!({})),
                    1 => assert!(
                        response["result"]["tools"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .any(|t| t["name"] == "read_delivery_diagnostics")
                    ),
                    6 => assert_eq!(response["error"]["code"], -32602),
                    7 => assert_eq!(response["error"]["code"], -32601),
                    _ => unreachable!(),
                }
            }
        }
        assert_eq!(std::fs::read_dir(root.path())?.count(), 0);
        drop(client);
        tokio::time::timeout(std::time::Duration::from_secs(5), task).await???;
        Ok(())
    }

    #[test]
    fn diagnostics_tool_is_discoverable_and_read_only() {
        let tools = definitions();
        let diagnostic = tools
            .iter()
            .find(|t| t["name"] == "read_delivery_diagnostics")
            .unwrap();
        assert_eq!(diagnostic["annotations"]["readOnlyHint"], true);
        assert_eq!(diagnostic["annotations"]["destructiveHint"], false);
        assert_eq!(diagnostic["inputSchema"]["required"], json!(["session_id"]));
        assert_eq!(diagnostic["inputSchema"]["additionalProperties"], false);
        assert_eq!(
            diagnostic["inputSchema"]["properties"]
                .as_object()
                .unwrap()
                .len(),
            2
        );
        // Exercise the same SDK conversion used when constructing the server.
        let _: Vec<Tool> = serde_json::from_value(json!(tools)).unwrap();
    }
}
