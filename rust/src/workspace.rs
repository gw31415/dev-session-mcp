use crate::config;
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

pub const MAX_OUTPUT: usize = 65536;
pub fn state_dir() -> Result<PathBuf> {
    Ok(std::env::var_os("DEV_SESSION_MCP_STATE_DIR")
        .map(PathBuf::from)
        .unwrap_or(
            dirs::home_dir()
                .context("HOME is required")?
                .join(".local/state/dev-session-mcp"),
        ))
}
#[derive(Serialize, Deserialize)]
pub struct WorkerSpec {
    pub command: Vec<String>,
    pub cwd: PathBuf,
    pub env: HashMap<String, String>,
}
pub struct Workspace {
    state: PathBuf,
    cwd: PathBuf,
    broker: crate::broker::Client,
    _backend: std::fs::File,
    session_operations: tokio::sync::Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

pub fn clean_environment() -> Result<()> {
    ensure!(
        !std::env::vars_os().any(|(k, _)| {
            let k = k.to_string_lossy();
            k.starts_with("CONTROL_PLANE_")
                || k.starts_with("TUNNEL_")
                || k.starts_with("MCP_")
                || k == "OPENAI_ADMIN_KEY"
                || k == "CREDENTIALS_DIRECTORY"
        }),
        "transport credentials/environment detected; use a clean environment and separate OS user"
    );
    Ok(())
}
pub fn safe_environment() -> HashMap<String, String> {
    let mut result = HashMap::from([
        (
            "PATH".into(),
            std::env::var("PATH").unwrap_or("/usr/local/bin:/usr/bin:/bin".into()),
        ),
        (
            "HOME".into(),
            dirs::home_dir()
                .unwrap_or(PathBuf::from("/tmp"))
                .display()
                .to_string(),
        ),
        ("LANG".into(), "C.UTF-8".into()),
        ("TERM".into(), "xterm-256color".into()),
    ]);
    for k in ["USER", "LOGNAME", "SHELL", "TMPDIR", "XDG_STATE_HOME"] {
        if let Ok(v) = std::env::var(k) {
            result.insert(k.into(), v);
        }
    }
    result
}
pub fn text<'a>(args: &'a Value, key: &str) -> Result<&'a str> {
    let value = args[key]
        .as_str()
        .with_context(|| format!("missing string {key}"))?;
    ensure!(
        value.len() <= MAX_OUTPUT && !value.contains('\0'),
        "invalid or oversized string"
    );
    Ok(value)
}
fn output_limit(args: &Value) -> Result<usize> {
    let size = match args.get("max_output_bytes") {
        Some(v) => v.as_u64().context("max_output_bytes must be an integer")? as usize,
        None => 16384,
    };
    ensure!(
        (256..=MAX_OUTPUT).contains(&size),
        "max_output_bytes must be 256..65536"
    );
    Ok(size)
}
pub fn bounded(text: &str, limit: usize, from_end: bool) -> (String, bool) {
    if text.len() <= limit {
        return (text.to_owned(), false);
    }
    if from_end {
        let mut start = text.len() - limit;
        while !text.is_char_boundary(start) {
            start += 1;
        }
        (text[start..].into(), true)
    } else {
        let marker = "\n[output truncated by dev-session-mcp]";
        let mut end = limit.saturating_sub(marker.len());
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        (
            format!(
                "{}{}",
                &text[..end],
                if limit >= marker.len() { marker } else { "" }
            ),
            true,
        )
    }
}

impl Workspace {
    pub async fn new() -> Result<Self> {
        clean_environment()?;
        let home = dirs::home_dir().context("HOME is required")?;
        let state = state_dir()?;
        ensure!(state.is_absolute(), "state directory must be absolute");
        let socket = state.join("broker.sock");
        ensure!(
            socket.as_os_str().len() <= 100,
            "state path exceeds Unix socket limit"
        );
        tokio::fs::create_dir_all(state.join("sessions")).await?;
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o700)).await?;
        // One owner for the ordinary upstream job handles; no editing/workflow lock.
        use std::os::fd::AsRawFd;
        let backend = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(state.join("rust-backend.lock"))?;
        ensure!(
            unsafe { libc::flock(backend.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0,
            "Rust backend already running for this state directory"
        );
        let broker = crate::broker::Client::new(&state).await?;
        Ok(Self {
            state,
            broker,
            _backend: backend,
            session_operations: tokio::sync::Mutex::new(HashMap::new()),
            cwd: std::env::var_os("DEV_SESSION_MCP_DEFAULT_CWD")
                .map(PathBuf::from)
                .unwrap_or(home),
        })
    }
    fn session_dir(&self, id: &str) -> Result<PathBuf> {
        config::validate_session_id(id)?;
        Ok(self.state.join("sessions").join(id))
    }
    fn job_dir(&self, id: &str, job: &str) -> Result<PathBuf> {
        config::validate_session_id(job)?;
        Ok(self.session_dir(id)?.join("jobs").join(job))
    }
    async fn operation_lock(&self, id: &str) -> Result<Arc<tokio::sync::Mutex<()>>> {
        config::validate_session_id(id)?;
        Ok(self
            .session_operations
            .lock()
            .await
            .entry(id.into())
            .or_default()
            .clone())
    }
    async fn pointer(&self, id: &str, name: &str) -> Result<Option<String>> {
        let path = self.session_dir(id)?.join(format!("{name}.json"));
        match tokio::fs::read(path).await {
            Ok(bytes) => {
                let record: Value = serde_json::from_slice(&bytes)?;
                let job = text(&record, "job_id")?.to_owned();
                self.job(id, &job).await?;
                Ok(Some(job))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }
    async fn select(&self, id: &str, name: &str, job: &str) -> Result<()> {
        let dir = self.session_dir(id)?;
        let temporary = dir.join(format!("{name}-{}.tmp", uuid::Uuid::new_v4()));
        tokio::fs::write(&temporary, serde_json::to_vec(&json!({"job_id":job}))?).await?;
        tokio::fs::rename(temporary, dir.join(format!("{name}.json"))).await?;
        Ok(())
    }
    async fn ensure_terminal(&self, id: &str, cwd: &Path) -> Result<String> {
        let previous = self.pointer(id, "terminal").await?;
        if let Some(job) = &previous {
            if self.status(id, job).await?["status"] == "running" {
                return Ok(job.clone());
            }
        }
        let active = self.pointer(id, "current").await?;
        let job = self
            .start(
                id,
                "shell",
                vec![
                    "/bin/bash".into(),
                    "--noprofile".into(),
                    "--norc".into(),
                    "-i".into(),
                ],
                cwd,
            )
            .await?;
        let job = text(&job, "job_id")?.to_owned();
        self.select(id, "terminal", &job).await?;
        if active.is_none() || active == previous {
            self.select(id, "current", &job).await?;
        }
        Ok(job)
    }
    async fn command_args(&self, id: &str, args: &Value) -> Result<Value> {
        let job = match args.get("job_id") {
            Some(_) => text(args, "job_id")?.to_owned(),
            None => self
                .pointer(id, "current")
                .await?
                .context("no active command; connect_session or run_command first")?,
        };
        let record = self.job(id, &job).await?;
        ensure!(
            record["kind"] != "approvals",
            "approval terminal is not a command job"
        );
        let mut resolved = args.clone();
        resolved["job_id"] = json!(job);
        Ok(resolved)
    }
    async fn status(&self, id: &str, job: &str) -> Result<Value> {
        let mut result = self
            .broker
            .call(json!({"op":"status","session_id":id,"job_id":job}))
            .await?;
        for key in ["output", "output_bytes", "truncated"] {
            result.as_object_mut().unwrap().remove(key);
        }
        Ok(result)
    }
    async fn start(&self, id: &str, kind: &str, command: Vec<String>, cwd: &Path) -> Result<Value> {
        ensure!(
            !command.is_empty()
                && !command[0].is_empty()
                && command.len() <= 256
                && serde_json::to_vec(&command)?.len() <= MAX_OUTPUT
                && command.iter().all(|s| !s.contains('\0')),
            "invalid command argv"
        );
        let job = uuid::Uuid::new_v4().to_string();
        let dir = self.job_dir(id, &job)?;
        tokio::fs::create_dir_all(&dir).await?;
        let record = json!({"session_id":id,"job_id":job,"kind":kind,"created_at":std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs(),"cwd":cwd});
        tokio::fs::write(dir.join("job.json"), serde_json::to_vec(&record)?).await?;
        if let Err(error) = self
            .broker
            .call(json!({"op":"start","session_id":id,"job_id":job,"command":command,"cwd":cwd}))
            .await
        {
            let _ = tokio::fs::remove_dir_all(&dir).await;
            return Err(error);
        }
        Ok(json!({"session_id":id,"job_id":job}))
    }
    async fn job(&self, id: &str, job: &str) -> Result<Value> {
        config::load_session(id).await?;
        Ok(serde_json::from_slice(
            &tokio::fs::read(self.job_dir(id, job)?.join("job.json")).await?,
        )?)
    }
    pub async fn session_info(&self, id: &str) -> Result<Value> {
        let session = config::load_session(id).await?;
        let mut jobs = Vec::new();
        if let Ok(mut entries) = tokio::fs::read_dir(self.session_dir(id)?.join("jobs")).await {
            while let Some(entry) = entries.next_entry().await? {
                let job = entry.file_name().to_string_lossy().into_owned();
                let mut record = self.job(id, &job).await?;
                record
                    .as_object_mut()
                    .unwrap()
                    .extend(self.status(id, &job).await?.as_object().unwrap().clone());
                jobs.push(record);
            }
        }
        let mut result = serde_json::to_value(session)?;
        let object = result.as_object_mut().unwrap();
        object.insert("session_id".into(), json!(id));
        object.insert("jobs".into(), json!(jobs));
        object.insert(
            "terminal_job_id".into(),
            json!(self.pointer(id, "terminal").await?),
        );
        object.insert(
            "active_job_id".into(),
            json!(self.pointer(id, "current").await?),
        );
        Ok(result)
    }
    pub async fn call(&self, name: &str, a: &Value) -> Result<Value> {
        if name == "list_sessions" {
            let mut sessions = Vec::new();
            if let Ok(mut entries) =
                tokio::fs::read_dir(config::state_dir()?.join("sessions")).await
            {
                while let Some(entry) = entries.next_entry().await? {
                    if let Some(id) = entry
                        .file_name()
                        .to_str()
                        .and_then(|v| v.strip_suffix(".json"))
                    {
                        sessions.push(self.session_info(id).await?);
                    }
                }
            }
            return Ok(json!({"sessions":sessions}));
        }
        if name == "create_session" {
            let id = a
                .get("session_id")
                .map(|_| text(a, "session_id").map(str::to_owned))
                .transpose()?
                .unwrap_or(uuid::Uuid::new_v4().to_string());
            config::validate_session_id(&id)?;
            let operation = self.operation_lock(&id).await?;
            let _operation = operation.lock().await;
            ensure!(
                !config::session_path(&id)?.exists(),
                "session already exists; use connect_session"
            );
            let cwd = config::canonical_directory(
                &a.get("cwd")
                    .map(|_| text(a, "cwd").map(PathBuf::from))
                    .transpose()?
                    .unwrap_or(self.cwd.clone()),
            )?;
            self.start(
                &id,
                "approvals",
                vec![
                    std::env::current_exe()?.display().to_string(),
                    "start".into(),
                    id.clone(),
                ],
                &cwd,
            )
            .await?;
            for _ in 0..50 {
                if config::session_path(&id)?.is_file() {
                    self.ensure_terminal(&id, &cwd).await?;
                    return self.session_info(&id).await;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            anyhow::bail!("session terminal did not create metadata");
        }
        let id = text(a, "session_id")?;
        let operation = self.operation_lock(id).await?;
        let _operation = operation.lock().await;
        let session = config::load_session(id).await?;
        match name {
            "connect_session" => {
                self.ensure_terminal(id, &session.cwd).await?;
                self.session_info(id).await
            }
            "run_command" => {
                output_limit(a)?;
                let job = match a.get("command") {
                    None => {
                        ensure!(a.get("cwd").is_none(), "cwd requires command argv");
                        self.ensure_terminal(id, &session.cwd).await?
                    }
                    Some(value) => {
                        let command = serde_json::from_value(value.clone())?;
                        let path = a
                            .get("cwd")
                            .map(|_| text(a, "cwd").map(PathBuf::from))
                            .transpose()?
                            .unwrap_or(PathBuf::from("."));
                        let cwd = config::canonical_directory(&session.cwd.join(path))?;
                        let job = self.start(id, "terminal", command, &cwd).await?;
                        text(&job, "job_id")?.to_owned()
                    }
                };
                self.select(id, "current", &job).await?;
                let mut args = a.clone();
                args["job_id"] = json!(job);
                self.poll(&args).await
            }
            "read_output" => self.poll(&self.command_args(id, a).await?).await,
            "send_stdin" | "resize_command" | "stop_command" => {
                output_limit(a)?;
                let mut args = self.command_args(id, a).await?;
                args["op"] = json!(match name {
                    "send_stdin" => "send",
                    "resize_command" => "resize",
                    _ => "stop",
                });
                self.broker.call(args.clone()).await?;
                self.poll(&args).await
            }
            "close_session" => {
                let info = self.session_info(id).await?;
                for job in info["jobs"].as_array().unwrap() {
                    let job = job["job_id"].as_str().unwrap();
                    self.broker
                        .call(json!({"op":"forget","session_id":id,"job_id":job}))
                        .await?;
                }
                tokio::fs::remove_file(config::session_path(id)?).await?;
                let dir = self.session_dir(id)?;
                if dir.join("memo.md").exists() {
                    // Removing the memo API must not delete existing users' notes.
                    if dir.join("jobs").exists() {
                        tokio::fs::remove_dir_all(dir.join("jobs")).await?;
                    }
                    for name in ["terminal.json", "current.json"] {
                        let path = dir.join(name);
                        if path.exists() {
                            tokio::fs::remove_file(path).await?;
                        }
                    }
                } else if dir.exists() {
                    tokio::fs::remove_dir_all(dir).await?;
                }
                Ok(json!({"session_id":id,"status":"closed"}))
            }
            _ => anyhow::bail!("unknown extension tool"),
        }
    }
    async fn poll(&self, args: &Value) -> Result<Value> {
        let id = text(args, "session_id")?;
        let job = text(args, "job_id")?;
        self.job(id, job).await?;
        let mut result = self.broker.call(json!({"op":"poll","session_id":id,"job_id":job,"max_output_bytes":output_limit(args)?})).await?;
        result["session_id"] = json!(id);
        result["job_id"] = json!(job);
        Ok(result)
    }
}
