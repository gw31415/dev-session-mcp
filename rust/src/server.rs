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
        "Stop only this session's managed executions and remove its session metadata. Preserve project files.",
        json!({"session_id":id}),
        &["session_id"],
        false,
    );
    add(
        "start_execution",
        "Start argv once. host uses FULL OS-user filesystem/network rights without approval; sandbox restricts writes to session roots and disables network, and requires io=pipes. PTY merges stdout/stderr. Subscribe to execution.events for batched push; read_execution is for reconnection/gaps. Returns the execution record. No automatic shell or implicit selected job.",
        json!({"session_id":id,"command":{"type":"array","items":text,"minItems":1,"maxItems":256},"profile":{"type":"string","enum":["sandbox","host"]},"io":{"type":"string","enum":["pty","pipes"],"default":"pty"},"cwd":path}),
        &["session_id", "command", "profile"],
        false,
    );
    add(
        "input_execution",
        "Queue literal UTF-8 stdin. Include newline to submit a line. Acceptance is distinct from OS write; Events report written or delivery_unknown. Never automatically resend input after a lost response or broker crash.",
        json!({"execution_id":id,"text":text}),
        &["execution_id", "text"],
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
        "Recover execution state and sequenced output after reconnect or an event gap. Pass the last cursor; deduplicate by sequence. History is bounded, catch_up_required marks loss, more means another bounded page remains. This is a recovery API; ordinary updates arrive through Events.",
        json!({"execution_id":id,"cursor":id}),
        &["execution_id"],
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
    tools.push(crate::files::tool());
    tools
}
impl ServerHandler for Server {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build()).with_server_info(Implementation::new(env!("CARGO_PKG_NAME"),env!("CARGO_PKG_VERSION"))).with_instructions("Explicit project sessions and executions. Use execution.events subscriptions for batched output/state/exit; read_execution only for catch-up. Host execution has full OS-user rights; sandbox execution disables network. Input acceptance is not an application acknowledgement. No implicit shell, default execution, or legacy API.")
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
        let mut result = ListToolsResult::default();
        result.tools = self.tools.clone();
        Ok(result)
    }
    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _: RequestContext<RoleServer>,
    ) -> std::result::Result<CallToolResponse, McpError> {
        if self.get_tool(&request.name).is_none() {
            return Err(McpError::invalid_params("unknown tool", None));
        }
        let mut args = json!(request.arguments.unwrap_or_default());
        args["op"] = json!(request.name);
        let result = match self.broker.call(args).await {
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
