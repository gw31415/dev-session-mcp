use crate::config;
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::{io::AsyncWriteExt, process::Command};

pub const MAX_OUTPUT: usize = 65536;
#[derive(Serialize, Deserialize)]
pub struct WorkerSpec {
    pub command: Vec<String>,
    pub cwd: PathBuf,
    pub env: HashMap<String, String>,
}
pub struct Workspace {
    state: PathBuf,
    cwd: PathBuf,
    tmux: String,
    socket: PathBuf,
    tmux_config: PathBuf,
    _backend: std::fs::File,
    create_session: tokio::sync::Mutex<()>,
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
        let marker = "\n[output truncated by oci-dev-mcp]";
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
        let state = std::env::var_os("OCI_DEV_STATE_DIR")
            .map(PathBuf::from)
            .unwrap_or(home.join(".local/state/oci-dev-mcp"));
        ensure!(state.is_absolute(), "state directory must be absolute");
        let socket = state.join("tmux.sock");
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
        let tmux_config = state.join("tmux.conf");
        tokio::fs::write(&tmux_config, "set -g history-limit 10000\nset -g remain-on-exit on\nset -g status off\nset -g default-shell /bin/bash\n").await?;
        let result = Self {
            state,
            socket,
            tmux_config,
            _backend: backend,
            create_session: tokio::sync::Mutex::new(()),
            cwd: std::env::var_os("OCI_DEV_DEFAULT_CWD")
                .map(PathBuf::from)
                .unwrap_or(home),
            tmux: std::env::var("OCI_DEV_TMUX_BIN").unwrap_or("tmux".into()),
        };
        let check = Command::new(&result.tmux)
            .arg("-V")
            .env_clear()
            .envs(safe_environment())
            .output()
            .await
            .context("tmux is required")?;
        ensure!(check.status.success(), "tmux is unavailable");
        Ok(result)
    }
    fn session_dir(&self, id: &str) -> Result<PathBuf> {
        config::validate_session_id(id)?;
        Ok(self.state.join("sessions").join(id))
    }
    fn job_dir(&self, id: &str, job: &str) -> Result<PathBuf> {
        config::validate_session_id(job)?;
        Ok(self.session_dir(id)?.join("jobs").join(job))
    }
    async fn tmux(
        &self,
        args: Vec<String>,
        input: Option<&[u8]>,
        allow_failure: bool,
    ) -> Result<std::process::Output> {
        use std::process::Stdio;
        let mut command = Command::new(&self.tmux);
        command
            .args(["-S"])
            .arg(&self.socket)
            .arg("-f")
            .arg(&self.tmux_config)
            .args(args)
            .env_clear()
            .envs(safe_environment())
            .kill_on_drop(true)
            .stdin(if input.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn()?;
        if let Some(data) = input {
            if let Some(mut stream) = child.stdin.take() {
                stream.write_all(data).await?;
            }
        }
        let out = tokio::time::timeout(Duration::from_secs(10), child.wait_with_output())
            .await
            .context("tmux operation timed out")??;
        // capture-pane is independently bounded by history and terminal width.
        ensure!(
            out.stdout.len() <= 4 * 1024 * 1024,
            "tmux response exceeded internal limit"
        );
        ensure!(
            allow_failure || out.status.success(),
            "tmux operation failed"
        );
        Ok(out)
    }
    async fn status(&self, job: &str) -> Result<Value> {
        config::validate_session_id(job)?;
        let exists = self
            .tmux(
                vec!["has-session".into(), "-t".into(), format!("=odm_{job}")],
                None,
                true,
            )
            .await?;
        if !exists.status.success() {
            return Ok(json!({"status":"unavailable", "exit_code":null}));
        }
        let out = self
            .tmux(
                vec![
                    "display-message".into(),
                    "-p".into(),
                    "-t".into(),
                    format!("odm_{job}:0.0"),
                    "#{pane_dead}|#{pane_dead_status}|#{pane_pid}|#{history_size}".into(),
                ],
                None,
                true,
            )
            .await?;
        if !out.status.success() {
            return Ok(json!({"status":"unavailable", "exit_code":null}));
        }
        let body = String::from_utf8_lossy(&out.stdout);
        let fields: Vec<_> = body.trim().split('|').collect();
        ensure!(fields.len() == 4, "invalid tmux status");
        Ok(
            json!({"status":if fields[0]=="1" {"completed"} else {"running"},"exit_code":if fields[0]=="1" {fields[1].parse::<i32>().ok()} else {None},"pid":fields[2].parse::<u32>().ok(),"history_lines":fields[3].parse::<u32>().ok()}),
        )
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
        let spec = dir.join("spec.json");
        tokio::fs::write(
            &spec,
            serde_json::to_vec(&WorkerSpec {
                command,
                cwd: cwd.into(),
                env: safe_environment(),
            })?,
        )
        .await?;
        self.tmux(
            vec![
                "new-session".into(),
                "-d".into(),
                "-s".into(),
                format!("odm_{job}"),
                "-x".into(),
                "160".into(),
                "-y".into(),
                "40".into(),
                "-c".into(),
                cwd.display().to_string(),
                std::env::current_exe()?.display().to_string(),
                "worker".into(),
                spec.display().to_string(),
            ],
            None,
            false,
        )
        .await?;
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
                    .extend(self.status(&job).await?.as_object().unwrap().clone());
                jobs.push(record);
            }
        }
        let mut result = serde_json::to_value(session)?;
        let object = result.as_object_mut().unwrap();
        object.insert("session_id".into(), json!(id));
        object.insert("mux_jobs".into(), json!(jobs));
        object.insert(
            "memo_available".into(),
            json!(self.session_dir(id)?.join("memo.md").is_file()),
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
            let _creating = self.create_session.lock().await;
            let id = a
                .get("session_id")
                .map(|_| text(a, "session_id").map(str::to_owned))
                .transpose()?
                .unwrap_or(uuid::Uuid::new_v4().to_string());
            config::validate_session_id(&id)?;
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
                    return self.session_info(&id).await;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            anyhow::bail!("session terminal did not create metadata");
        }
        let id = text(a, "session_id")?;
        let session = config::load_session(id).await?;
        match name {
            "connect_session" => self.session_info(id).await,
            "get_memo" => {
                let path = self.session_dir(id)?.join("memo.md");
                let memo = if path.exists() {
                    tokio::fs::read_to_string(path).await?
                } else {
                    String::new()
                };
                Ok(json!({"session_id":id,"text":memo}))
            }
            "set_memo" => {
                let memo = text(a, "text")?;
                let dir = self.session_dir(id)?;
                tokio::fs::create_dir_all(&dir).await?;
                let temporary = dir.join(format!("memo-{}.tmp", uuid::Uuid::new_v4()));
                tokio::fs::write(&temporary, memo).await?;
                tokio::fs::rename(&temporary, dir.join("memo.md")).await?;
                Ok(json!({"session_id":id,"saved":true,"bytes":memo.len()}))
            }
            "mux_open" => {
                let command = match a.get("command") {
                    Some(v) => serde_json::from_value(v.clone())?,
                    None => vec![
                        "/bin/bash".into(),
                        "--noprofile".into(),
                        "--norc".into(),
                        "-i".into(),
                    ],
                };
                let path = a
                    .get("cwd")
                    .map(|_| text(a, "cwd").map(PathBuf::from))
                    .transpose()?
                    .unwrap_or(PathBuf::from("."));
                let cwd = config::canonical_directory(&session.cwd.join(path))?;
                let job = self.start(id, "terminal", command, &cwd).await?;
                self.poll(&job).await
            }
            "mux_poll" => self.poll(a).await,
            "mux_send" => {
                let job = text(a, "job_id")?;
                self.job(id, job).await?;
                ensure!(
                    self.status(job).await?["status"] == "running",
                    "terminal is not running"
                );
                if a.get("text").is_some() {
                    let data = text(a, "text")?;
                    // Paste a private tmux buffer: no key interpretation or shell interpolation.
                    let buffer = format!("odm_{}", uuid::Uuid::new_v4());
                    self.tmux(
                        vec![
                            "load-buffer".into(),
                            "-b".into(),
                            buffer.clone(),
                            "-".into(),
                        ],
                        Some(data.as_bytes()),
                        false,
                    )
                    .await?;
                    self.tmux(
                        vec![
                            "paste-buffer".into(),
                            "-d".into(),
                            "-b".into(),
                            buffer,
                            "-t".into(),
                            format!("odm_{job}:0.0"),
                        ],
                        None,
                        false,
                    )
                    .await?;
                }
                if let Some(keys) = a.get("keys") {
                    let keys: Vec<String> = serde_json::from_value(keys.clone())?;
                    ensure!(
                        keys.len() <= 16
                            && keys.iter().all(|k| [
                                "Enter", "C-c", "C-d", "Escape", "Tab", "Up", "Down", "Left",
                                "Right"
                            ]
                            .contains(&k.as_str())),
                        "invalid terminal key"
                    );
                    if !keys.is_empty() {
                        let mut args =
                            vec!["send-keys".into(), "-t".into(), format!("odm_{job}:0.0")];
                        args.extend(keys);
                        self.tmux(args, None, false).await?;
                    }
                }
                self.poll(a).await
            }
            "mux_stop" => {
                let job = text(a, "job_id")?;
                let mut before = self.poll(a).await?;
                self.tmux(
                    vec!["kill-session".into(), "-t".into(), format!("=odm_{job}")],
                    None,
                    true,
                )
                .await?;
                before["status"] = json!("stopped");
                before["exit_code"] = Value::Null;
                Ok(before)
            }
            "close_session" => {
                let info = self.session_info(id).await?;
                for job in info["mux_jobs"].as_array().unwrap() {
                    let job = job["job_id"].as_str().unwrap();
                    self.tmux(
                        vec!["kill-session".into(), "-t".into(), format!("=odm_{job}")],
                        None,
                        true,
                    )
                    .await?;
                }
                tokio::fs::remove_file(config::session_path(id)?).await?;
                let dir = self.session_dir(id)?;
                if dir.exists() {
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
        let status = self.status(job).await?;
        let mut result =
            json!({"session_id":id,"job_id":job,"output":"","output_bytes":0,"truncated":false});
        result
            .as_object_mut()
            .unwrap()
            .extend(status.as_object().unwrap().clone());
        if status["status"] == "unavailable" {
            return Ok(result);
        }
        let out = self
            .tmux(
                vec![
                    "capture-pane".into(),
                    "-p".into(),
                    "-J".into(),
                    "-t".into(),
                    format!("odm_{job}:0.0"),
                    "-S".into(),
                    "-10000".into(),
                ],
                None,
                true,
            )
            .await?;
        if !out.status.success() {
            result["status"] = json!("unavailable");
            result["exit_code"] = Value::Null;
            return Ok(result);
        }
        let (output, truncated) = bounded(
            &String::from_utf8_lossy(&out.stdout),
            output_limit(args)?,
            true,
        );
        result["output_bytes"] = json!(output.len());
        result["output"] = json!(output);
        result["truncated"] = json!(truncated);
        Ok(result)
    }
}
