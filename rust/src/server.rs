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
            "Create a development session with an automatically managed persistent terminal. Existing IDs are not overwritten.",
            json!({"session_id":sid,"cwd":string}),
            &[],
            false,
        );
        add(
            "connect_session",
            "Reconnect a session and automatically reuse or restore its persistent terminal. Discover command job IDs; no editing lock.",
            json!({"session_id":sid}),
            &["session_id"],
            false,
        );
        add(
            "run_command",
            "Run persistent argv in this session; omitted command reuses its interactive shell. Selects the returned job for session-only stdin/output/stop. Other commands keep running. FULL service-user filesystem/network access without sandbox or approval. Use execute for sandboxed commands.",
            json!({"session_id":sid,"command":{"type":"array","items":string,"minItems":1,"maxItems":256},"cwd":string,"max_output_bytes":output}),
            &["session_id"],
            false,
        );
        add(
            "read_output",
            "Read bounded merged terminal output and exit status for this session's selected command. Optional job_id selects another command belonging to this session. Repeated terminal snapshot, not a stream.",
            json!({"session_id":sid,"job_id":sid,"max_output_bytes":output}),
            &["session_id"],
            true,
        );
        add(
            "send_stdin",
            "Send literal stdin to this session's selected command, with optional job_id for another command in this session. FULL service-user rights without sandbox or approval. Enter submits; C-c interrupts; C-d sends EOF.",
            json!({"session_id":sid,"job_id":sid,"text":string,"keys":{"type":"array","maxItems":16,"items":{"type":"string","enum":["Enter","C-c","C-d","Escape","Tab","Up","Down","Left","Right"]}},"max_output_bytes":output}),
            &["session_id"],
            false,
        );
        add(
            "stop_command",
            "Stop only the selected command (optional job_id) in this session. Other commands and the session remain. Detached/daemonized descendants need explicit process management.",
            json!({"session_id":sid,"job_id":sid}),
            &["session_id"],
            false,
        );
        add(
            "resize_command",
            "Resize the selected command terminal in this session (optional job_id). Does not restart the command.",
            json!({"session_id":sid,"job_id":sid,"rows":{"type":"integer","minimum":1,"maximum":1000},"cols":{"type":"integer","minimum":1,"maximum":1000}}),
            &["session_id", "rows", "cols"],
            false,
        );
        add(
            "close_session",
            "Stop all managed terminals/commands in this session and remove session metadata. Other sessions remain. Refuses while ordinary sandboxed jobs remain tracked.",
            json!({"session_id":sid}),
            &["session_id"],
            false,
        );
        definitions.push(crate::files::tool());
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
        let mut data = if name == "import_file" {
            crate::files::import(&args).await?
        } else {
            self.workspace.call(name, &args).await?
        };
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
            .with_server_info(Implementation::new(env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION")))
            .with_instructions("Create or connect a session; its persistent terminal is managed automatically. Use session_id with run_command, read_output, send_stdin and stop_command. Omit command in run_command to reuse the interactive shell. The most recent run selects the default job; use job_id for concurrent commands. These terminal tools have full OS-user/filesystem/network rights without a sandbox or approval. execute/start_command keep their sandbox contract and process-local job handles. Use ordinary files for handover; close_session ends all terminals in that session.")
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
        "dev-session-mcp listening on loopback {} (OAuth required)",
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
