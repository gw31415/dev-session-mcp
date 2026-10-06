use crate::{
    auth::{Auth, Settings, authenticate},
    mcp,
    workspace::{MAX_OUTPUT, Workspace, bounded},
};
use anyhow::Result;
use axum::{Router, middleware, routing::get};
use rmcp::{
    ErrorData as McpError, RoleServer, ServerHandler, ServiceExt,
    model::*,
    service::RequestContext,
    transport::{
        StreamableHttpServerConfig, StreamableHttpService,
        streamable_http_server::session::local::LocalSessionManager,
    },
};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::{Arc, Mutex},
};

#[derive(Clone)]
struct Tools {
    workspace: Arc<Workspace>,
    tracked: Arc<Mutex<HashMap<String, HashSet<String>>>>,
    tools: Arc<Vec<Tool>>,
}

impl Tools {
    async fn new() -> Result<Self> {
        let mut definitions = mcp::tools().as_array().unwrap().clone();
        let sid =
            json!({"type":"string","minLength":1,"maxLength":64,"pattern":"^[A-Za-z0-9_.-]+$"});
        let string = json!({"type":"string","maxLength":65536});
        let output = json!({"type":"integer","minimum":256,"maximum":65536});
        let mut add = |name: &str,
                       description: &str,
                       properties: Value,
                       required: &[&str],
                       read_only: bool| {
            definitions.push(json!({"name":name,"description":description,"inputSchema":{"type":"object","properties":properties,"required":required,"additionalProperties":false},"annotations":{"readOnlyHint":read_only,"destructiveHint":!read_only,"openWorldHint":true}}));
        };
        add(
            "list_sessions",
            "List development sessions, working directories and persistent terminals.",
            json!({}),
            &[],
            true,
        );
        add(
            "create_session",
            "Create a local-mcp session in tmux. Existing IDs are not overwritten.",
            json!({"session_id":sid,"cwd":string}),
            &[],
            false,
        );
        add(
            "connect_session",
            "Reconnect and discover persistent job IDs. No exclusive ownership or editing lock.",
            json!({"session_id":sid}),
            &["session_id"],
            true,
        );
        add(
            "get_memo",
            "Read an explicitly saved session memo.",
            json!({"session_id":sid}),
            &["session_id"],
            true,
        );
        add(
            "set_memo",
            "Replace the explicit session memo, up to 64 KiB.",
            json!({"session_id":sid,"text":string}),
            &["session_id", "text"],
            false,
        );
        add(
            "mux_open",
            "Open persistent argv or interactive bash in tmux. FULL service-user filesystem/network access without a sandbox or approval prompt. Use execute for sandboxed commands.",
            json!({"session_id":sid,"command":{"type":"array","items":string,"minItems":1,"maxItems":256},"cwd":string}),
            &["session_id"],
            false,
        );
        add(
            "mux_poll",
            "Return bounded merged terminal output and exit status. Repeated terminal snapshot, not a stream.",
            json!({"session_id":sid,"job_id":sid,"max_output_bytes":output}),
            &["session_id", "job_id"],
            true,
        );
        add(
            "mux_send",
            "Send literal stdin text and terminal keys; Enter submits, C-c interrupts, C-d sends EOF.",
            json!({"session_id":sid,"job_id":sid,"text":string,"keys":{"type":"array","maxItems":16,"items":{"type":"string","enum":["Enter","C-c","C-d","Escape","Tab","Up","Down","Left","Right"]}},"max_output_bytes":output}),
            &["session_id", "job_id"],
            false,
        );
        add(
            "mux_stop",
            "Stop a tmux terminal. Detached/daemonized descendants need explicit process management.",
            json!({"session_id":sid,"job_id":sid}),
            &["session_id", "job_id"],
            false,
        );
        add(
            "close_session",
            "Stop persistent terminals and remove session/memo metadata. Refuses while ordinary upstream jobs remain tracked.",
            json!({"session_id":sid}),
            &["session_id"],
            false,
        );
        Ok(Self {
            workspace: Arc::new(Workspace::new().await?),
            tracked: Arc::new(Mutex::new(HashMap::new())),
            tools: Arc::new(serde_json::from_value(json!(definitions))?),
        })
    }
    async fn dispatch(&self, name: &str, args: Value) -> Result<CallToolResult> {
        let upstream = mcp::tools()
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v["name"] == name);
        if upstream {
            let mut result = mcp::call_tool(&json!({"name":name,"arguments":args})).await?;
            let id = args["session_id"].as_str().unwrap_or("").to_owned();
            let data = result["content"][0]["text"]
                .as_str()
                .and_then(|s| serde_json::from_str::<Value>(s).ok());
            {
                let mut tracked = self.tracked.lock().unwrap();
                if ["execute", "start_command"].contains(&name) {
                    if let Some(job) = data.as_ref().and_then(|v| v["job_id"].as_str()) {
                        tracked.entry(id.clone()).or_default().insert(job.into());
                    }
                }
                if ["poll_job", "stop_job"].contains(&name)
                    && !data.as_ref().is_some_and(|v| v["status"] == "running")
                {
                    if let Some(job) = args["job_id"].as_str() {
                        if let Some(jobs) = tracked.get_mut(&id) {
                            jobs.remove(job);
                        }
                    }
                }
            }
            let mut left = MAX_OUTPUT;
            if let Some(content) = result["content"].as_array_mut() {
                for block in content {
                    if let Some(text) = block["text"].as_str() {
                        let (text, _) = bounded(text, left, false);
                        left -= text.len();
                        block["text"] = json!(text);
                    }
                }
            }
            let mut result: CallToolResult = serde_json::from_value(result)?;
            result.result_type = Some(ResultType::COMPLETE);
            return Ok(result);
        }
        if name == "close_session" {
            let tracked = self.tracked.lock().unwrap();
            anyhow::ensure!(
                !tracked
                    .get(args["session_id"].as_str().unwrap_or(""))
                    .is_some_and(|jobs| !jobs.is_empty()),
                "upstream jobs remain; use poll_job/stop_job first"
            );
        }
        let mut data = self.workspace.call(name, &args).await?;
        if name == "connect_session" || name == "list_sessions" {
            let tracked = self.tracked.lock().unwrap();
            let annotate = |v: &mut Value| {
                let id = v["session_id"].as_str().unwrap_or("");
                let jobs: Vec<_> = tracked
                    .get(id)
                    .map(|s| s.iter().map(|j| json!({"job_id":j})).collect())
                    .unwrap_or_default();
                v["upstream_jobs"] = json!(jobs);
            };
            if name == "connect_session" {
                annotate(&mut data);
            } else {
                for v in data["sessions"].as_array_mut().unwrap() {
                    annotate(v);
                }
            }
        }
        Ok(CallToolResult::success(vec![ContentBlock::text(
            serde_json::to_string(&data)?,
        )]))
    }
}
impl ServerHandler for Tools {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_instructions("Use list_sessions, create_session or connect_session. Persist work with tmux mux tools; save an explicit memo for handover. Ordinary execute jobs survive HTTP disconnect, but require this server process. mux tools have full OS-user and network rights.")
    }
    fn get_tool(&self, name: &str) -> Option<Tool> {
        self.tools.iter().find(|t| t.name == name).cloned()
    }
    async fn list_tools(
        &self,
        _: Option<PaginatedRequestParams>,
        _: RequestContext<RoleServer>,
    ) -> std::result::Result<ListToolsResult, McpError> {
        let mut result = ListToolsResult::default();
        result.tools = self.tools.as_ref().clone();
        Ok(result)
    }
    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _: RequestContext<RoleServer>,
    ) -> std::result::Result<CallToolResponse, McpError> {
        let result = self
            .dispatch(&request.name, json!(request.arguments.unwrap_or_default()))
            .await;
        let result = match result {
            Ok(r) => r,
            Err(e) => CallToolResult::error(vec![ContentBlock::text(
                bounded(&format!("{e:#}"), 1024, false).0,
            )]),
        };
        Ok(result.into())
    }
}

pub async fn stdio() -> Result<()> {
    Tools::new()
        .await?
        .serve(rmcp::transport::stdio())
        .await?
        .waiting()
        .await?;
    Ok(())
}
pub async fn http(path: PathBuf) -> Result<()> {
    crate::workspace::clean_environment()?;
    let settings: Settings = serde_json::from_slice(&tokio::fs::read(path).await?)?;
    let auth = Auth::load(settings).await?;
    let tools = Tools::new().await?;
    let resource = url::Url::parse(&auth.settings.resource)?;
    let mut hosts = vec![
        resource.host_str().unwrap().into(),
        auth.settings.listen.to_string(),
    ];
    if let Some(port) = resource.port() {
        hosts.push(format!("{}:{port}", resource.host_str().unwrap()));
    }
    let mut origins = auth.settings.allowed_origins.clone();
    // Explicit port avoids rmcp's deprecated portless matching behavior.
    origins.push(format!(
        "{}://{}:{}",
        resource.scheme(),
        resource.host().unwrap(),
        resource.port_or_known_default().unwrap()
    ));
    let mut config = StreamableHttpServerConfig::default();
    config.legacy_session_mode = true;
    config.json_response = true;
    config.max_request_body_bytes = 1024 * 1024;
    let config = config
        .with_allowed_hosts(hosts)
        .with_allowed_origins(origins)
        .enforce_origin_validation();
    let cancellation = config.cancellation_token.clone();
    let service = StreamableHttpService::new(
        move || Ok(tools.clone()),
        Arc::new(LocalSessionManager::default()),
        config,
    );
    let protected = Router::new()
        .route_service("/mcp", service)
        .layer(middleware::from_fn_with_state(auth.clone(), authenticate));
    let metadata = auth.metadata.clone();
    let root = metadata.clone();
    let app = Router::new()
        .route(
            "/.well-known/oauth-protected-resource/mcp",
            get(move || {
                let doc = metadata.clone();
                async move { axum::Json(doc) }
            }),
        )
        .route(
            "/.well-known/oauth-protected-resource",
            get(move || {
                let doc = root.clone();
                async move { axum::Json(doc) }
            }),
        )
        .merge(protected);
    let listener = tokio::net::TcpListener::bind(auth.settings.listen).await?;
    eprintln!(
        "oci-dev-mcp listening on loopback {} (OAuth required)",
        auth.settings.listen
    );
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            let mut term =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                    .expect("signal handler");
            tokio::select! { _ = tokio::signal::ctrl_c() => {}, _ = term.recv() => {} }
            cancellation.cancel();
        })
        .await?;
    Ok(())
}
