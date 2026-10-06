//! Private, same-UID PTY holder. Independent of MCP frontend lifetime.
use crate::{
    config,
    workspace::{MAX_OUTPUT, bounded, clean_environment, safe_environment},
};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, VecDeque},
    os::{
        fd::AsRawFd,
        unix::{
            fs::{FileTypeExt, MetadataExt, PermissionsExt},
            process::CommandExt,
        },
    },
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
    sync::{Notify, mpsc},
};

const FRAME_LIMIT: u64 = 1024 * 1024;
type Jobs = Arc<tokio::sync::Mutex<HashMap<(String, String), Arc<Job>>>>;
struct Buffer {
    bytes: VecDeque<u8>,
    total: u64,
    status: &'static str,
    exit_code: Option<i32>,
}
struct Job {
    pid: u32,
    output: Mutex<Buffer>,
    input: tokio::sync::Mutex<Option<pty_process::OwnedWritePty>>,
    stop: mpsc::Sender<()>,
}

pub struct Client {
    socket: PathBuf,
}

/// Local human interface only: never exposed as an MCP approval tool.
pub async fn approval_console(session_id: &str) -> Result<()> {
    clean_environment()?;
    config::load_session(session_id).await?;
    let state = crate::workspace::state_dir()?;
    let client = Client {
        socket: state.join("broker.sock"),
    };
    let mut entries =
        tokio::fs::read_dir(state.join("sessions").join(session_id).join("jobs")).await?;
    let mut selected = None;
    while let Some(entry) = entries.next_entry().await? {
        let record: Value =
            serde_json::from_slice(&tokio::fs::read(entry.path().join("job.json")).await?)?;
        if record["kind"] == "approvals" {
            let job = record["job_id"].as_str().context("invalid approval job")?;
            if client
                .call(json!({"op":"status","session_id":session_id,"job_id":job}))
                .await?["status"]
                == "running"
            {
                selected = Some(job.to_owned());
                break;
            }
        }
    }
    let job = selected.context("no running approval terminal for this session")?;
    eprintln!(
        "Local approval console for {session_id}. Ctrl-C or stdin EOF detaches; the session keeps running."
    );
    let mut input = BufReader::new(tokio::io::stdin()).lines();
    let mut stdout = tokio::io::stdout();
    let mut printed = String::new();
    let mut tick = tokio::time::interval(Duration::from_millis(100));
    loop {
        tokio::select! {
            _ = tick.tick() => {
                let response = client.call(json!({"op":"poll","session_id":session_id,"job_id":job,"max_output_bytes":MAX_OUTPUT})).await?;
                let output = response["output"].as_str().context("invalid approval output")?;
                // Ring truncation may require redisplaying the retained tail.
                let new = output.strip_prefix(&printed).unwrap_or(output);
                stdout.write_all(new.as_bytes()).await?;
                stdout.flush().await?;
                printed = output.to_owned();
                ensure!(response["status"] == "running", "approval terminal stopped");
            }
            line = input.next_line() => {
                let Some(line) = line? else { return Ok(()) };
                ensure!(line.len() < MAX_OUTPUT, "approval input too large");
                client.call(json!({"op":"send","session_id":session_id,"job_id":job,"text":format!("{line}\n")})).await?;
            }
            _ = tokio::signal::ctrl_c() => return Ok(()),
        }
    }
}
impl Client {
    pub async fn new(state: &Path) -> Result<Self> {
        let client = Self {
            socket: state.join("broker.sock"),
        };
        if client.call(json!({"op":"ping"})).await.is_ok() {
            return Ok(client);
        }
        let mut command = std::process::Command::new(std::env::current_exe()?);
        command
            .args(["broker"])
            .arg(state)
            .env_clear()
            .envs(safe_environment())
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        // Detach only our private holder, never the user's existing services.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = command
            .spawn()
            .context("could not start private PTY broker")?;
        for _ in 0..100 {
            if client.call(json!({"op":"ping"})).await.is_ok() {
                return Ok(client);
            }
            if child.try_wait()?.is_some() {
                anyhow::bail!("private PTY broker exited during startup");
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        anyhow::bail!("private PTY broker startup timed out")
    }
    pub async fn call(&self, request: Value) -> Result<Value> {
        tokio::time::timeout(Duration::from_secs(5), async {
            let metadata = std::fs::symlink_metadata(&self.socket)?;
            ensure!(
                metadata.file_type().is_socket()
                    && metadata.uid() == unsafe { libc::geteuid() }
                    && metadata.mode() & 0o077 == 0,
                "broker socket must be private and owned by this OS user"
            );
            let mut socket = UnixStream::connect(&self.socket).await?;
            ensure!(
                socket.peer_cred()?.uid() == unsafe { libc::geteuid() },
                "broker UID mismatch"
            );
            let mut bytes = serde_json::to_vec(&request)?;
            ensure!(
                bytes.len() < FRAME_LIMIT as usize,
                "broker request too large"
            );
            bytes.push(b'\n');
            socket.write_all(&bytes).await?;
            let mut response = Vec::new();
            BufReader::new(socket.take(FRAME_LIMIT + 1))
                .read_until(b'\n', &mut response)
                .await?;
            ensure!(
                response.len() <= FRAME_LIMIT as usize && response.last() == Some(&b'\n'),
                "invalid broker response frame"
            );
            let response: Value = serde_json::from_slice(&response)?;
            ensure!(
                response["ok"] == true,
                "{}",
                response["error"]
                    .as_str()
                    .unwrap_or("broker operation failed")
            );
            Ok(response["result"].clone())
        })
        .await
        .context("broker request timed out")?
    }
}

fn key(request: &Value) -> Result<(String, String)> {
    let id = request["session_id"]
        .as_str()
        .context("missing session_id")?;
    let job = request["job_id"].as_str().context("missing job_id")?;
    config::validate_session_id(id)?;
    config::validate_session_id(job)?;
    Ok((id.into(), job.into()))
}
fn snapshot(job: Option<&Arc<Job>>, limit: usize) -> Value {
    let Some(job) = job else {
        return json!({"status":"unavailable","exit_code":null,"output":"","output_bytes":0,"truncated":false});
    };
    let buffer = job.output.lock().unwrap();
    let raw: Vec<u8> = buffer.bytes.iter().copied().collect();
    let stripped = strip_ansi_escapes::strip(&raw);
    let text = String::from_utf8_lossy(&stripped).replace("\r\n", "\n");
    let (output, truncated) = bounded(&text, limit, true);
    json!({"status":buffer.status,"exit_code":buffer.exit_code,"pid":job.pid,"output_bytes":output.len(),"output":output,"truncated":truncated || buffer.total > raw.len() as u64})
}
async fn stop(job: &Arc<Job>) -> Result<()> {
    if job.output.lock().unwrap().status != "running" {
        return Ok(());
    }
    let _ = job.stop.try_send(());
    for _ in 0..100 {
        if job.output.lock().unwrap().status != "running" {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    anyhow::bail!("command did not stop")
}
async fn handle(request: Value, jobs: &Jobs, shutdown: &Notify) -> Result<Value> {
    let op = request["op"].as_str().context("missing broker operation")?;
    if op == "ping" {
        return Ok(json!({"pid":std::process::id(),"protocol":1}));
    }
    if op == "shutdown" {
        shutdown.notify_one();
        return Ok(json!({"status":"stopping"}));
    }
    let key = key(&request)?;
    if op == "start" {
        let mut jobs = jobs.lock().await;
        ensure!(!jobs.contains_key(&key), "command already exists");
        let command: Vec<String> = serde_json::from_value(request["command"].clone())?;
        ensure!(
            !command.is_empty()
                && !command[0].is_empty()
                && command.len() <= 256
                && serde_json::to_vec(&command)?.len() <= MAX_OUTPUT
                && command.iter().all(|s| !s.contains('\0')),
            "invalid command argv"
        );
        let cwd = config::canonical_directory(Path::new(
            request["cwd"].as_str().context("missing cwd")?,
        ))?;
        let (pty, pts) = pty_process::open()?;
        pty.resize(pty_process::Size::new(40, 160))?;
        let mut child = pty_process::Command::new(&command[0])
            .args(&command[1..])
            .current_dir(cwd)
            .env_clear()
            .envs(safe_environment())
            .kill_on_drop(true)
            .spawn(pts)?;
        let pid = child.id().context("missing command PID")?;
        let (mut reader, writer) = pty.into_split();
        let (stop_tx, mut stop_rx) = mpsc::channel(1);
        let job = Arc::new(Job {
            pid,
            output: Mutex::new(Buffer {
                bytes: VecDeque::new(),
                total: 0,
                status: "running",
                exit_code: None,
            }),
            input: tokio::sync::Mutex::new(Some(writer)),
            stop: stop_tx,
        });
        jobs.insert(key, job.clone());
        let output_job = job.clone();
        let mut reader_task = tokio::spawn(async move {
            let mut bytes = [0; 8192];
            loop {
                match reader.read(&mut bytes).await {
                    Ok(0) => break,
                    Ok(count) => {
                        let mut output = output_job.output.lock().unwrap();
                        output.total = output.total.saturating_add(count as u64);
                        output.bytes.extend(&bytes[..count]);
                        let excess = output.bytes.len().saturating_sub(MAX_OUTPUT);
                        output.bytes.drain(..excess);
                    }
                    // Linux signals PTY slave closure with EIO.
                    Err(error) if error.raw_os_error() == Some(libc::EIO) => break,
                    Err(_) => break,
                }
            }
        });
        tokio::spawn(async move {
            let (status, stopped) = tokio::select! {
                result = child.wait() => (result, false),
                _ = stop_rx.recv() => {
                    unsafe { libc::kill(-(pid as i32), libc::SIGKILL); }
                    (child.wait().await, true)
                }
            };
            // Drain final output before publishing completion; a detached child
            // retaining the slave cannot keep completion waiting forever.
            if tokio::time::timeout(Duration::from_millis(500), &mut reader_task)
                .await
                .is_err()
            {
                reader_task.abort();
            }
            job.input.lock().await.take();
            let mut output = job.output.lock().unwrap();
            output.status = if stopped {
                "stopped"
            } else if status.is_ok() {
                "completed"
            } else {
                "unavailable"
            };
            output.exit_code = status.ok().and_then(|s| s.code());
        });
        return Ok(json!({"pid":pid}));
    }
    let job = jobs.lock().await.get(&key).cloned();
    let limit = request
        .get("max_output_bytes")
        .map(|v| v.as_u64().context("invalid output limit"))
        .transpose()?
        .unwrap_or(16384) as usize;
    ensure!(
        (256..=MAX_OUTPUT).contains(&limit),
        "output limit must be 256..65536"
    );
    if op == "poll" || op == "status" {
        return Ok(snapshot(job.as_ref(), limit));
    }
    if op == "forget" {
        if let Some(job) = &job {
            stop(job).await?;
        }
        jobs.lock().await.remove(&key);
        return Ok(json!({"status":"unavailable"}));
    }
    let job = job.context("command unavailable")?;
    if op == "stop" {
        stop(&job).await?;
        return Ok(snapshot(Some(&job), limit));
    }
    ensure!(
        job.output.lock().unwrap().status == "running",
        "command is not running"
    );
    let mut input = job.input.lock().await;
    let writer = input.as_mut().context("command input closed")?;
    match op {
        "send" => {
            let text = request
                .get("text")
                .map(|v| v.as_str().context("invalid stdin text"))
                .transpose()?
                .unwrap_or("");
            ensure!(
                text.len() <= MAX_OUTPUT && !text.contains('\0'),
                "stdin too large or invalid"
            );
            let keys: Vec<String> =
                serde_json::from_value(request.get("keys").cloned().unwrap_or(json!([])))?;
            ensure!(keys.len() <= 16, "too many terminal keys");
            let mut bytes = text.as_bytes().to_vec();
            for key in keys {
                bytes.extend_from_slice(match key.as_str() {
                    "Enter" => b"\r",
                    "C-c" => b"\x03",
                    "C-d" => b"\x04",
                    "Escape" => b"\x1b",
                    "Tab" => b"\t",
                    "Up" => b"\x1b[A",
                    "Down" => b"\x1b[B",
                    "Left" => b"\x1b[D",
                    "Right" => b"\x1b[C",
                    _ => anyhow::bail!("invalid terminal key"),
                });
            }
            writer.write_all(&bytes).await?;
        }
        "resize" => {
            let rows = request["rows"].as_u64().context("missing rows")?;
            let cols = request["cols"].as_u64().context("missing cols")?;
            ensure!(
                (1..=1000).contains(&rows) && (1..=1000).contains(&cols),
                "terminal dimensions must be 1..1000"
            );
            writer.resize(pty_process::Size::new(rows as u16, cols as u16))?;
        }
        _ => anyhow::bail!("unknown broker operation"),
    }
    drop(input);
    Ok(snapshot(Some(&job), limit))
}
pub async fn run(state: PathBuf) -> Result<()> {
    clean_environment()?;
    ensure!(state.is_absolute(), "broker state path must be absolute");
    std::fs::create_dir_all(&state)?;
    std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o700))?;
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(state.join("broker.lock"))?;
    ensure!(
        unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0,
        "PTY broker already running"
    );
    let socket = state.join("broker.sock");
    match std::fs::remove_file(&socket) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    let listener = UnixListener::bind(&socket)?;
    std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))?;
    std::fs::write(state.join("broker.pid"), std::process::id().to_string())?;
    let jobs: Jobs = Arc::new(tokio::sync::Mutex::new(HashMap::new()));
    let shutdown = Arc::new(Notify::new());
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    loop {
        let stream = tokio::select! { pair = listener.accept() => pair?.0, _ = shutdown.notified()=>break, _=terminate.recv()=>break };
        if stream.peer_cred()?.uid() != unsafe { libc::geteuid() } {
            continue;
        }
        let jobs = jobs.clone();
        let shutdown = shutdown.clone();
        tokio::spawn(async move {
            let (reader, mut writer) = stream.into_split();
            let response = tokio::time::timeout(Duration::from_secs(5), async {
                let mut bytes = Vec::new();
                BufReader::new(reader.take(FRAME_LIMIT + 1))
                    .read_until(b'\n', &mut bytes)
                    .await?;
                ensure!(
                    bytes.len() <= FRAME_LIMIT as usize && bytes.last() == Some(&b'\n'),
                    "invalid request frame"
                );
                handle(serde_json::from_slice(&bytes)?, &jobs, &shutdown).await
            })
            .await;
            let value = match response {
                Ok(Ok(v)) => json!({"ok":true,"result":v}),
                Ok(Err(e)) => json!({"ok":false,"error":e.to_string()}),
                Err(_) => json!({"ok":false,"error":"broker operation timed out"}),
            };
            if let Ok(mut bytes) = serde_json::to_vec(&value) {
                bytes.push(b'\n');
                let _ = writer.write_all(&bytes).await;
            }
        });
    }
    let held: Vec<_> = jobs.lock().await.values().cloned().collect();
    for job in held {
        let _ = stop(&job).await;
    }
    let _ = std::fs::remove_file(socket);
    let _ = std::fs::remove_file(state.join("broker.pid"));
    Ok(())
}
