//! Single owner of processes, execution records and the bounded event journal.
use crate::{
    config, sandbox,
    workspace::{self, MAX_OUTPUT, text},
};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, VecDeque},
    os::{
        fd::AsRawFd,
        unix::fs::{FileTypeExt, MetadataExt, PermissionsExt},
    },
    path::{Path, PathBuf},
    process::Stdio,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
    sync::{Notify, mpsc},
};

const FRAME: u64 = 2 * 1024 * 1024;
const HISTORY_BYTES: usize = 1024 * 1024;
const HISTORY_EVENTS: usize = 1024;
const EXECUTIONS: usize = 64;
const BATCH_BYTES: usize = 32 * 1024;
/// Bumped whenever frontend and broker operations change incompatibly.
const PROTOCOL: u64 = 3;

type Shared = Arc<Mutex<Ledger>>;
struct Execution {
    record: Value,
    actions: mpsc::Sender<Action>,
    signals: mpsc::Sender<i32>,
}
enum Action {
    Input(Vec<u8>, u64, bool),
    Resize(u16, u16),
}
struct Ledger {
    epoch: String,
    sequence: u64,
    events: VecDeque<Value>,
    bytes: usize,
    executions: HashMap<String, Execution>,
    changed: Arc<Notify>,
    durable: crate::durable::Store,
}
impl Ledger {
    fn new() -> Result<Self> {
        Ok(Self {
            epoch: uuid::Uuid::new_v4().simple().to_string(),
            sequence: 0,
            events: VecDeque::new(),
            bytes: 0,
            executions: HashMap::new(),
            changed: Arc::new(Notify::new()),
            durable: crate::durable::Store::open(workspace::state_dir()?.join("recovery.json"))?,
        })
    }
    fn append(&mut self, execution: &str, kind: &str, data: Value) -> u64 {
        let _ = self.append_checked(execution, kind, data);
        self.sequence
    }
    fn append_checked(&mut self, execution: &str, kind: &str, data: Value) -> Result<u64> {
        self.sequence += 1;
        let seq = self.sequence;
        let record = self.executions.get_mut(execution).expect("owned execution");
        record.record["cursor"] = json!(format!("{}:{seq}", self.epoch));
        let event = json!({"sequence":seq,"execution_id":execution,"session_id":record.record["session_id"],"kind":kind,"timestamp":crate::events::timestamp(),"data":data});
        self.bytes += serde_json::to_vec(&event).unwrap().len();
        let persisted = self.durable.append(record.record.clone(), event.clone());
        self.events.push_back(event);
        while self.bytes > HISTORY_BYTES || self.events.len() > HISTORY_EVENTS {
            self.bytes -= serde_json::to_vec(&self.events.pop_front().unwrap())
                .unwrap()
                .len();
        }
        self.changed.notify_waiters();
        persisted?;
        Ok(seq)
    }
    fn read(&self, args: &Value) -> Result<Value> {
        let execution = args.get("execution_id").and_then(Value::as_str);
        let session = args.get("session_id").and_then(Value::as_str);
        if let Some(id) = execution {
            ensure!(
                self.durable.record(id).is_some(),
                "execution history unavailable or retired; never restart automatically"
            );
        }
        if execution.is_some_and(|id| !self.executions.contains_key(id)) {
            return self.durable.read(args);
        }
        let record = execution
            .map(|id| -> Result<Value> {
                let mut record = self
                    .executions
                    .get(id)
                    .context("execution unavailable; never use a bare PID")?
                    .record
                    .clone();
                let durable = self
                    .durable
                    .record(id)
                    .context("execution evidence unavailable")?;
                // Checkpoints own work metadata; the live copy may predate them.
                // Keep live process state, input receipts and cursor unchanged.
                for field in [
                    "purpose",
                    "completion_condition",
                    "retain_work",
                    "work_completed",
                ] {
                    if let Some(value) = durable.get(field) {
                        record[field] = value.clone();
                    } else {
                        record.as_object_mut().unwrap().remove(field);
                    }
                }
                Ok(record)
            })
            .transpose()?;
        let earliest = self
            .events
            .front()
            .and_then(|v| v["sequence"].as_u64())
            .unwrap_or(self.sequence + 1);
        let supplied = args.get("cursor").and_then(Value::as_str);
        let mut position = self.sequence;
        let mut gap = false;
        if let Some(cursor) = supplied {
            let (epoch, seq) = cursor.rsplit_once(':').context("invalid cursor")?;
            let seq: u64 = seq.parse()?;
            gap = epoch != self.epoch || seq.saturating_add(1) < earliest || seq > self.sequence;
            position = if gap { earliest.saturating_sub(1) } else { seq };
        }
        let mut events = Vec::new();
        let mut bytes = 0;
        let mut end = position;
        let mut more = false;
        for event in &self.events {
            let seq = event["sequence"].as_u64().unwrap();
            if seq <= position {
                continue;
            }
            let matches = execution.is_none_or(|id| event["execution_id"] == id)
                && session.is_none_or(|id| event["session_id"] == id);
            if matches {
                let size = serde_json::to_vec(event)?.len();
                if bytes + size > BATCH_BYTES {
                    more = true;
                    break;
                }
                bytes += size;
                events.push(event.clone());
            }
            end = seq;
        }
        Ok(
            json!({"execution":record,"events":events,"cursor":format!("{}:{end}",self.epoch),"earliest_cursor":format!("{}:{}",self.epoch,earliest.saturating_sub(1)),"catch_up_required":gap,"more":more,"persistence_ok":self.durable.healthy(),"durable_cursor":execution.map(|id| self.durable.durable_cursor(id)),"observed_cursor":format!("{}:{}",self.epoch,self.sequence)}),
        )
    }
}
#[derive(Clone)]
pub struct Client {
    socket: PathBuf,
    uid: u32,
    /// Frontends restart a missing broker; the broker's own client never does.
    spawn: bool,
}
impl Client {
    pub fn at(socket: PathBuf) -> Self {
        Self {
            socket,
            uid: unsafe { libc::geteuid() },
            spawn: false,
        }
    }

    pub async fn new() -> Result<Self> {
        let configured = std::env::var_os("DEV_SESSION_MCP_BROKER_SOCKET");
        let client = Self {
            spawn: configured.is_none(),
            ..Self::at(
                configured
                    .map(PathBuf::from)
                    .unwrap_or(workspace::state_dir()?.join("broker.sock")),
            )
        };
        client.ensure_broker().await?;
        Ok(client)
    }

    async fn ensure_broker(&self) -> Result<()> {
        if let Ok(info) = self.request(json!({"op":"ping"})).await {
            ensure!(
                info["protocol"] == PROTOCOL,
                "running broker speaks protocol {} (expected {PROTOCOL}); stop it after its executions finish, then reconnect",
                info["protocol"]
            );
            return Ok(());
        }
        ensure!(
            self.spawn,
            "configured broker unavailable; no alternate broker started"
        );
        let state = workspace::state_dir()?;
        let mut command = std::process::Command::new(std::env::current_exe()?);
        command
            .arg("broker")
            .env_clear()
            .envs(workspace::safe_environment())
            .env("DEV_SESSION_MCP_STATE_DIR", &state)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        for key in ["DEV_SESSION_MCP_FILE_ORIGINS", crate::profiles::ENV] {
            if let Some(value) = std::env::var_os(key) {
                command.env(key, value);
            }
        }
        use std::os::unix::process::CommandExt;
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = command.spawn()?;
        // Reap the broker if it exits early; once running it outlives us (setsid).
        for _ in 0..100 {
            if self.request(json!({"op":"ping"})).await.is_ok() {
                return Ok(());
            }
            if child.try_wait()?.is_some() {
                // Another frontend may have won the broker lock meanwhile.
                return self.request(json!({"op":"ping"})).await.map(|_| ());
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        anyhow::bail!("broker startup timed out")
    }

    pub async fn call(&self, request: Value) -> Result<Value> {
        match self.connect().await {
            Ok(stream) => self.exchange(stream, request).await,
            Err(_) if self.spawn => {
                self.ensure_broker().await?;
                self.exchange(self.connect().await?, request).await
            }
            Err(error) => Err(error),
        }
    }
    async fn request(&self, request: Value) -> Result<Value> {
        self.exchange(self.connect().await?, request).await
    }
    async fn connect(&self) -> Result<UnixStream> {
        let metadata = std::fs::symlink_metadata(&self.socket)?;
        ensure!(
            metadata.file_type().is_socket()
                && metadata.uid() == self.uid
                && metadata.mode() & 0o077 == 0,
            "broker socket must be private and owned by this UID"
        );
        let stream = UnixStream::connect(&self.socket).await?;
        ensure!(stream.peer_cred()?.uid() == self.uid, "broker UID mismatch");
        Ok(stream)
    }
    async fn exchange(&self, mut stream: UnixStream, request: Value) -> Result<Value> {
        let mut body = serde_json::to_vec(&request)?;
        ensure!(body.len() < FRAME as usize, "request too large");
        body.push(b'\n');
        stream.write_all(&body).await?;
        let mut response = Vec::new();
        let mut reader = BufReader::new(stream.take(FRAME + 1));
        let read = reader.read_until(b'\n', &mut response);
        match request["op"].as_str() {
            Some("wait_events") => {
                read.await?;
            }
            // Callback verification has its own DNS/connect/response deadlines.
            Some("events_subscribe") => {
                tokio::time::timeout(Duration::from_secs(40), read).await??;
            }
            _ => {
                tokio::time::timeout(Duration::from_secs(20), read).await??;
            }
        }
        ensure!(
            response.len() <= FRAME as usize && response.last() == Some(&b'\n'),
            "invalid broker frame"
        );
        let value: Value = serde_json::from_slice(&response)?;
        ensure!(
            value["ok"] == true,
            "{}",
            value["error"].as_str().unwrap_or("broker operation failed")
        );
        Ok(value["result"].clone())
    }
}
async fn pump<R: AsyncRead + Unpin>(
    mut reader: R,
    ledger: Shared,
    id: String,
    stream: &'static str,
) {
    let mut buffer = [0; 4096];
    let mut pending = Vec::new();
    loop {
        let count = match reader.read(&mut buffer).await {
            Ok(n) => n,
            Err(_) => 0,
        };
        if count == 0 {
            if !pending.is_empty() {
                ledger.lock().unwrap().append(
                    &id,
                    "output",
                    json!({"stream":stream,"text":String::from_utf8_lossy(&pending)}),
                );
            }
            break;
        }
        pending.extend_from_slice(&buffer[..count]);
        let mut output = String::new();
        loop {
            match std::str::from_utf8(&pending) {
                Ok(text) => {
                    output.push_str(text);
                    pending.clear();
                    break;
                }
                Err(error) => {
                    let valid = error.valid_up_to();
                    output.push_str(std::str::from_utf8(&pending[..valid]).unwrap());
                    if let Some(invalid) = error.error_len() {
                        output.push('\u{fffd}');
                        pending.drain(..valid + invalid);
                    } else {
                        pending.drain(..valid);
                        break;
                    }
                }
            }
        }
        if !output.is_empty() {
            ledger
                .lock()
                .unwrap()
                .append(&id, "output", json!({"stream":stream,"text":output}));
        }
    }
}
enum Input {
    Pty(pty_process::OwnedWritePty),
    Pipe(Option<tokio::process::ChildStdin>),
}
impl Input {
    async fn write(&mut self, data: &[u8]) -> std::io::Result<()> {
        if data.is_empty() {
            return Ok(());
        }
        match self {
            Self::Pty(w) => w.write_all(data).await,
            Self::Pipe(w) => match w {
                Some(w) => w.write_all(data).await,
                None => Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "stdin is closed",
                )),
            },
        }
    }
    fn close(&mut self) {
        if let Self::Pipe(w) = self {
            w.take();
        }
    }
    fn resize(&self, rows: u16, cols: u16) -> Result<()> {
        match self {
            Self::Pty(w) => {
                w.resize(pty_process::Size::new(rows, cols))?;
                Ok(())
            }
            Self::Pipe(_) => anyhow::bail!("resize requires a PTY"),
        }
    }
}
async fn start(args: &Value, ledger: Shared) -> Result<Value> {
    ensure!(
        args.as_object()
            .is_some_and(|map| map.keys().all(|key| matches!(
                key.as_str(),
                "op" | "session_id"
                    | "command"
                    | "profile"
                    | "io"
                    | "cwd"
                    | "idempotency_key"
                    | "key_generation"
                    | "work_id"
                    | "purpose"
                    | "completion_condition"
            ))),
        "unknown start argument"
    );
    let mut canonical = args.clone();
    canonical
        .as_object_mut()
        .context("expected arguments")?
        .remove("op");
    canonical.as_object_mut().unwrap().remove("idempotency_key");
    canonical.as_object_mut().unwrap().remove("key_generation");
    let generation = args
        .get("key_generation")
        .map(|_| crate::durable::short(args, "key_generation", 128))
        .transpose()?;
    ensure!(
        generation.is_none() || args.get("idempotency_key").is_some(),
        "key_generation requires idempotency_key"
    );
    if canonical.get("io").is_none() {
        canonical["io"] = json!("pty");
    }
    let fingerprint = crate::durable::digest(&canonical)?;
    let key = args
        .get("idempotency_key")
        .map(|_| -> Result<String> {
            let key = crate::durable::short(args, "idempotency_key", 128)?;
            if let Some(generation) = &generation {
                crate::durable::digest(&json!([text(args, "session_id")?, generation, key]))
            } else {
                crate::durable::digest(&json!([text(args, "session_id")?, key]))
            }
        })
        .transpose()?;
    if let Some(key) = &key {
        if let Some(record) =
            ledger
                .lock()
                .unwrap()
                .durable
                .replay(key, &fingerprint, generation.as_deref())?
        {
            return Ok(record);
        }
    }
    let session = config::load_session(text(args, "session_id")?).await?;
    ensure!(
        session.state == config::SessionState::Open,
        "session is closing"
    );
    let command: Vec<String> = serde_json::from_value(args["command"].clone())?;
    ensure!(
        !command.is_empty()
            && command.len() <= 256
            && command.iter().all(|s| !s.contains('\0'))
            && serde_json::to_vec(&command)?.len() <= MAX_OUTPUT,
        "invalid argv"
    );
    let profile = text(args, "profile")?;
    ensure!(
        ["host", "sandbox"].contains(&profile) || crate::profiles::custom(profile),
        "profile must be host, sandbox or admin:<id>"
    );
    let io = args.get("io").and_then(Value::as_str).unwrap_or("pty");
    ensure!(["pty", "pipes"].contains(&io), "io must be pty or pipes");
    ensure!(
        profile == "host" || io == "pipes",
        "sandbox and admin profiles require pipes; host PTY retains full OS-user rights"
    );
    let cwd = if let Some(path) = args.get("cwd").and_then(Value::as_str) {
        config::canonical_directory(Path::new(path))?
    } else {
        session.cwd.clone()
    };
    ensure!(
        profile == "host"
            || session
                .permitted_directories
                .iter()
                .any(|root| cwd.starts_with(root)),
        "sandbox cwd must lie within session permitted roots"
    );
    // Resolve exactly once for a NEW start, after idempotent replay. Edits never
    // change a running execution or cause an old key to execute a new definition.
    let admin = if crate::profiles::custom(profile) {
        Some(crate::profiles::resolve(profile)?)
    } else {
        None
    };
    let mut prepared = if let Some(def) = &admin {
        Some(def.command(&command, &cwd, &session.cwd, &session.permitted_directories)?)
    } else {
        None
    };
    let mut state = ledger.lock().unwrap();
    if state.executions.len() >= EXECUTIONS {
        let candidate = state
            .executions
            .iter()
            .filter(|(_, e)| e.record["status"] != "running")
            .min_by_key(|(_, e)| e.record["started_sequence"].as_u64().unwrap_or(0))
            .map(|(id, _)| id.clone());
        if let Some(id) = candidate {
            state.executions.remove(&id);
        } else {
            anyhow::bail!("64 live executions reached; stop one before starting another");
        }
    }
    // Persist the recoverable ID before any spawn. A lost ACK must never cause a new child.
    let id = format!("{}:{}", state.epoch, uuid::Uuid::new_v4().simple());
    let work_id = if args.get("work_id").is_some() {
        crate::durable::short(args, "work_id", 128)?
    } else {
        id.clone()
    };
    let purpose = args
        .get("purpose")
        .map(|_| crate::durable::short(args, "purpose", 2048))
        .transpose()?;
    let condition = args
        .get("completion_condition")
        .map(|_| crate::durable::short(args, "completion_condition", 2048))
        .transpose()?;
    let initial_cursor = format!("{}:{}", state.epoch, state.sequence);
    let retain_work = ["work_id", "purpose", "completion_condition"]
        .iter()
        .any(|key| args.get(key).is_some());
    let mut reserved = json!({"execution_id":id,"session_id":session.id,"work_id":work_id,
        "purpose":purpose,"completion_condition":condition,"command":command,
        "status":"starting","exit_code":null,"cursor":initial_cursor,"initial_cursor":initial_cursor,
        "retain_work":retain_work,
        "profile":profile,"io":io,"session_state":"open","stdin_closed":false,"key_generation":generation});
    if let Some(def) = &admin {
        reserved["execution_policy"] = def.metadata()?;
    }
    state.durable.reserve(reserved.clone(), key, fingerprint)?;
    let Ledger {
        executions,
        durable,
        ..
    } = &mut *state;
    executions.retain(|id, _| durable.record(id).is_some());
    drop(state);
    let (mut child, mut input, readers) = if io == "pty" {
        let (pty, pts) = pty_process::open()?;
        pty.resize(pty_process::Size::new(24, 80))?;
        let child = pty_process::Command::new(&command[0])
            .args(&command[1..])
            .current_dir(&cwd)
            .env_clear()
            .envs(workspace::safe_environment())
            .kill_on_drop(true)
            .spawn(pts)?;
        let (reader, writer) = pty.into_split();
        (
            child,
            Input::Pty(writer),
            vec![(
                Box::new(reader) as Box<dyn AsyncRead + Unpin + Send>,
                "terminal",
            )],
        )
    } else {
        let mut process = if let Some(process) = prepared.take() {
            process
        } else if profile == "sandbox" {
            sandbox::command(&command, &cwd, &session.permitted_directories)?
        } else {
            let mut p = tokio::process::Command::new(&command[0]);
            p.args(&command[1..])
                .current_dir(&cwd)
                .env_clear()
                .envs(workspace::safe_environment());
            p
        };
        process
            .process_group(0)
            .kill_on_drop(true)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = process.spawn()?;
        let input = Input::Pipe(child.stdin.take());
        let readers = vec![
            (
                Box::new(child.stdout.take().unwrap()) as Box<dyn AsyncRead + Unpin + Send>,
                "stdout",
            ),
            (
                Box::new(child.stderr.take().unwrap()) as Box<dyn AsyncRead + Unpin + Send>,
                "stderr",
            ),
        ];
        (child, input, readers)
    };
    let pid = child.id().context("missing child PID")?;
    let ticks = std::fs::read_to_string(format!("/proc/{pid}/stat"))?
        .rsplit_once(')')
        .context("invalid proc stat")?
        .1
        .split_whitespace()
        .nth(19)
        .context("missing start ticks")?
        .to_owned();
    let (tx, mut rx) = mpsc::channel(8);
    let (signals, mut signal_rx) = mpsc::channel(8);
    let initial;
    {
        let mut state = ledger.lock().unwrap();
        let mut record = reserved;
        record.as_object_mut().unwrap().remove("command");
        record["pid"] = json!(pid);
        record["process_start_ticks"] = json!(ticks);
        record["status"] = json!("running");
        record["started_sequence"] = json!(state.sequence + 1);
        record["start_cursor"] = json!(format!("{}:{}", state.epoch, state.sequence + 1));
        state.executions.insert(
            id.clone(),
            Execution {
                record,
                actions: tx,
                signals,
            },
        );
        state.append(&id, "started", json!({"profile":profile,"io":io,"pid":pid}));
        // The start cursor must precede output even if a short child finishes
        // before the RPC response is delivered.
        initial = state.executions[&id].record.clone();
    }
    let mut pumps = Vec::new();
    for (reader, stream) in readers {
        pumps.push(tokio::spawn(pump(
            reader,
            ledger.clone(),
            id.clone(),
            stream,
        )));
    }
    let owned_id = id.clone();
    let held = ledger.clone();
    tokio::spawn(async move {
        let status = loop {
            tokio::select! {
                biased;
                result=child.wait()=>break result,
                Some(signal)=signal_rx.recv()=>apply_signal(&held,&owned_id,pid,signal),
                action=rx.recv()=>match action {
                    Some(Action::Input(bytes,receipt,close))=> {
                        // Interrupting a blocked write may leave partial input. Never
                        // retry it; report uncertainty while handling signals promptly.
                        let written=tokio::select! {
                            biased;
                            Some(signal)=signal_rx.recv()=> {apply_signal(&held,&owned_id,pid,signal);false},
                            result=tokio::time::timeout(Duration::from_secs(2),input.write(&bytes))=>result.is_ok_and(|r|r.is_ok()),
                        };
                        if close {input.close();}
                        let delivery=if written{"written"}else{"delivery_unknown"};
                        let mut state=held.lock().unwrap();
                        let record=&mut state.executions.get_mut(&owned_id).unwrap().record;
                        if close {record["stdin_closed"]=json!(true);}
                        if record["input"]["sequence"]==receipt {record["input"]["delivery"]=json!(delivery);record["input"]["stdin_closed"]=json!(close);}
                        state.append(&owned_id,"input",json!({"input_sequence":receipt,"delivery":delivery,"stdin_closed":close}));
                    }
                    Some(Action::Resize(rows,cols))=> {
                        let ok=input.resize(rows,cols).is_ok();
                        held.lock().unwrap().append(&owned_id,"resize",json!({"rows":rows,"cols":cols,"applied":ok}));
                    }
                    None=> {apply_signal(&held,&owned_id,pid,libc::SIGKILL);break child.wait().await;}
                }
            }
        };
        while let Ok(action) = rx.try_recv() {
            if let Action::Input(_, receipt, _) = action {
                let mut state = held.lock().unwrap();
                let record = &mut state.executions.get_mut(&owned_id).unwrap().record;
                if record["input"]["sequence"] == receipt {
                    record["input"]["delivery"] = json!("delivery_unknown");
                }
                state.append(
                    &owned_id,
                    "input",
                    json!({"input_sequence":receipt,"delivery":"delivery_unknown"}),
                );
            }
        }
        drop(input);
        let mut output_complete = true;
        for mut reader in pumps {
            if tokio::time::timeout(Duration::from_millis(500), &mut reader)
                .await
                .is_err()
            {
                output_complete = false;
                reader.abort();
            }
        }
        use std::os::unix::process::ExitStatusExt;
        let code = status.as_ref().ok().and_then(|s| s.code());
        let signal = status.as_ref().ok().and_then(|s| s.signal());
        let confirmed = status.is_ok();
        let mut state = held.lock().unwrap();
        if let Some(execution) = state.executions.get_mut(&owned_id) {
            execution.record["status"] = json!(if confirmed {
                "exited"
            } else {
                "outcome_unknown"
            });
            execution.record["exit_signal"] = json!(signal);
            execution.record["output_complete"] = json!(output_complete);
            execution.record["stdin_closed"] = json!(true);
            execution.record["exit_code"] = json!(code);
        }
        state.append(&owned_id, "exit", json!({"exit_code":code}));
    });
    ensure!(
        ledger.lock().unwrap().durable.healthy(),
        "start result persistence uncertain; retain execution ID via list_sessions, do not resend"
    );
    Ok(initial)
}
fn apply_signal(ledger: &Shared, id: &str, pid: u32, signal: i32) {
    // Only this actor reaps the child, so its PID cannot be reused while we
    // signal its process group, including while interrupting a stdin write.
    let sent = unsafe { libc::kill(-(pid as i32), signal) } == 0;
    ledger
        .lock()
        .unwrap()
        .append(id, "signal", json!({"signal":signal,"sent":sent}));
}
async fn inspect_event_session(id: &str, ledger: &Shared) -> Result<Value> {
    config::validate_session_id(id)?;
    if let Ok(session) = config::load_session(id).await {
        return Ok(json!(session));
    }
    let state = ledger.lock().unwrap();
    ensure!(
        state
            .executions
            .values()
            .any(|e| e.record["session_id"] == id && e.record["session_state"] == "closed"),
        "session history unavailable"
    );
    Ok(json!({"id":id,"state":"closed"}))
}
async fn handle(
    args: Value,
    ledger: Shared,
    starts: Arc<tokio::sync::Mutex<()>>,
    shutdown: Arc<Notify>,
    events: crate::events::Events,
) -> Result<Value> {
    match text(&args, "op")? {
        "ping" => Ok(
            json!({"pid":std::process::id(),"epoch":ledger.lock().unwrap().epoch,"protocol":PROTOCOL}),
        ),
        "events_subscribe" => events.subscribe(args["params"].clone()).await,
        "events_unsubscribe" => events.unsubscribe(args["params"].clone()).await,
        "shutdown" => {
            shutdown.notify_one();
            Ok(json!({"status":"stopping"}))
        }
        "open_session" => {
            let _guard = starts.lock().await;
            let cwd = config::canonical_directory(Path::new(text(&args, "cwd")?))?;
            let session = config::create_session(&cwd, None).await?;
            let state = ledger.lock().unwrap();
            let mut value = json!(session);
            value["execution_profiles"] = crate::profiles::catalog();
            value["recovery"] = state.durable.recovery_info();
            value["executions"] = json!(
                state
                    .durable
                    .records()
                    .into_iter()
                    .filter(|v| v["session_id"] == session.id)
                    .collect::<Vec<_>>()
            );
            value["checkpoints"] = json!(
                state
                    .durable
                    .checkpoints()
                    .into_iter()
                    .filter(|v| v["session_id"] == session.id)
                    .collect::<Vec<_>>()
            );
            Ok(value)
        }
        "list_sessions" => {
            let mut entries = tokio::fs::read_dir(workspace::state_dir()?.join("sessions")).await?;
            let mut sessions = Vec::new();
            while let Some(entry) = entries.next_entry().await? {
                if sessions.len() >= 64 {
                    break;
                }
                if entry.path().extension().is_some_and(|s| s == "json") {
                    let data: Value =
                        serde_json::from_slice(&tokio::fs::read(entry.path()).await?)?;
                    sessions.push(data);
                }
            }
            let state = ledger.lock().unwrap();
            Ok(
                json!({"execution_profiles":crate::profiles::catalog(),"sessions":sessions,"executions":state.durable.records(),"checkpoints":state.durable.checkpoints(),"persistence_ok":state.durable.healthy(),"recovery":state.durable.recovery_info()}),
            )
        }
        "inspect_session" => Ok(json!(
            config::load_session(text(&args, "session_id")?).await?
        )),
        "inspect_event_session" => inspect_event_session(text(&args, "session_id")?, &ledger).await,
        "close_session" => {
            let _guard = starts.lock().await;
            let id = text(&args, "session_id")?;
            let path = config::session_path(id)?;
            if !path.exists() {
                return Ok(json!({"session_id":id,"closed":true,"pending":false}));
            }
            let mut session = config::load_session(id).await?;
            session.state = config::SessionState::Closing;
            config::save_session(&session).await?;
            let signals = {
                let mut state = ledger.lock().unwrap();
                let ids: Vec<_> = state
                    .executions
                    .iter()
                    .filter(|(_, e)| {
                        e.record["session_id"] == id && e.record["session_state"] != "closed"
                    })
                    .map(|(id, _)| id.clone())
                    .collect();
                for execution in ids {
                    if state.executions[&execution].record["session_state"] != "closing" {
                        state.executions.get_mut(&execution).unwrap().record["session_state"] =
                            json!("closing");
                        state.append(&execution, "closing", json!({"session_state":"closing"}));
                    }
                }
                state
                    .executions
                    .values()
                    .filter(|e| e.record["session_id"] == id && e.record["status"] == "running")
                    .map(|e| e.signals.clone())
                    .collect::<Vec<_>>()
            };
            for signal in signals {
                let _ = signal.try_send(libc::SIGKILL);
            }
            let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
            loop {
                let notify = ledger.lock().unwrap().changed.clone();
                let changed = notify.notified();
                tokio::pin!(changed);
                changed.as_mut().enable();
                let running = ledger
                    .lock()
                    .unwrap()
                    .executions
                    .values()
                    .any(|e| e.record["session_id"] == id && e.record["status"] == "running");
                if !running {
                    break;
                }
                if tokio::time::timeout_at(deadline, changed).await.is_err() {
                    return Ok(json!({"session_id":id,"closed":false,"pending":true}));
                }
            }
            {
                let mut state = ledger.lock().unwrap();
                let ids: Vec<_> = state
                    .executions
                    .iter()
                    .filter(|(_, e)| {
                        e.record["session_id"] == id && e.record["session_state"] != "closed"
                    })
                    .map(|(id, _)| id.clone())
                    .collect();
                for execution in ids {
                    state.executions.get_mut(&execution).unwrap().record["session_state"] =
                        json!("closed");
                    state.append(&execution, "closed", json!({"session_state":"closed"}));
                }
            }
            tokio::fs::remove_file(path).await?;
            ledger.lock().unwrap().changed.notify_waiters();
            Ok(json!({"session_id":id,"closed":true,"pending":false}))
        }
        "start_execution" => {
            let _guard = starts.lock().await;
            start(&args, ledger).await
        }
        "event_position" => ledger.lock().unwrap().read(&args),
        "read_execution" => {
            text(&args, "execution_id")?;
            let state = ledger.lock().unwrap();
            let mut reading = args.clone();
            if args["view"] == "summary" && args.get("cursor").is_none() {
                reading["cursor"] = state.durable.record(text(&args, "execution_id")?)
                    .context("execution unavailable; do not restart automatically")?["initial_cursor"].clone();
            }
            let data = state.read(&reading)?;
            crate::durable::project(data, &args)
        }
        "checkpoint_execution" => ledger.lock().unwrap().durable.checkpoint(&args),
        "execution_source" => ledger
            .lock()
            .unwrap()
            .durable
            .source(text(&args, "execution_id")?),
        "wait_events" => loop {
            let notify = ledger.lock().unwrap().changed.clone();
            let notified = notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            inspect_event_session(text(&args, "session_id")?, &ledger).await?;
            let data = ledger.lock().unwrap().read(&args)?;
            if data["catch_up_required"] == true
                || data["events"].as_array().is_some_and(|s| !s.is_empty())
            {
                break Ok(data);
            }
            notified.await;
        },
        "signal_execution" => {
            let id = text(&args, "execution_id")?;
            let state = ledger.lock().unwrap();
            let execution = state
                .executions
                .get(id)
                .context("execution unavailable; bare/stale PIDs are rejected")?;
            ensure!(
                execution.record["status"] == "running",
                "execution is not running"
            );
            let signal = match text(&args, "signal")? {
                "INT" => libc::SIGINT,
                "TERM" => libc::SIGTERM,
                "KILL" => libc::SIGKILL,
                _ => anyhow::bail!("signal must be INT, TERM or KILL"),
            };
            execution
                .signals
                .try_send(signal)
                .context("signal queue full or closed; signal not accepted")?;
            Ok(execution.record.clone())
        }
        "input_execution" | "resize_execution" => {
            let id = text(&args, "execution_id")?;
            let mut state = ledger.lock().unwrap();
            let execution = state
                .executions
                .get(id)
                .context("execution unavailable; bare/stale PIDs are rejected")?;
            ensure!(
                execution.record["status"] == "running",
                "execution is not running"
            );
            let sender = execution.actions.clone();
            let permit = sender
                .try_reserve()
                .context("execution input queue full or closed; action not accepted")?;
            let action = if args["op"] == "input_execution" {
                ensure!(
                    execution.record["session_state"] != "closing",
                    "session is closing"
                );
                let close = args
                    .get("close_stdin")
                    .map(|v| v.as_bool().context("close_stdin must be boolean"))
                    .transpose()?
                    .unwrap_or(false);
                ensure!(
                    !close || execution.record["io"] == "pipes",
                    "close_stdin closes a pipe; use literal Ctrl-D for a PTY"
                );
                ensure!(
                    args.get("text").is_some() || close,
                    "text or close_stdin is required"
                );
                let bytes = if args.get("text").is_some() {
                    text(&args, "text")?.as_bytes().to_vec()
                } else {
                    Vec::new()
                };
                if execution.record["stdin_closed"] == true {
                    ensure!(bytes.is_empty() && close, "stdin is closed");
                    return Ok(execution.record.clone());
                }
                let seq = state.sequence + 1;
                state.executions.get_mut(id).unwrap().record["input"] = json!({"sequence":seq,"delivery":"prepared","queued":false,"close_stdin":close});
                if state.append_checked(id, "input", json!({"input_sequence":seq,"delivery":"prepared","bytes":bytes.len(),"close_stdin":close})).is_err() {
                    let receipt = &mut state.executions.get_mut(id).unwrap().record["input"];
                    receipt["delivery"] = json!("not_sent");
                    receipt["receipt_durable"] = Value::Null;
                    state.append(id,"input",json!({"input_sequence":seq,"delivery":"not_sent","queued":false}));
                    anyhow::bail!("stdin not sent: durable receipt commit failed; queued=false. A prepared receipt recovered after restart is delivery_unknown; do not resend automatically");
                }
                permit.send(Action::Input(bytes, seq, close));
                let receipt = &mut state.executions.get_mut(id).unwrap().record["input"];
                receipt["delivery"] = json!("accepted");
                receipt["queued"] = json!(true);
                receipt["receipt_durable"] = json!(true);
                if state
                    .append_checked(
                        id,
                        "input",
                        json!({"input_sequence":seq,"delivery":"accepted","queued":true}),
                    )
                    .is_err()
                {
                    state.executions.get_mut(id).unwrap().record["input"]["delivery"] =
                        json!("delivery_unknown");
                    state.append(
                        id,
                        "input",
                        json!({"input_sequence":seq,"delivery":"delivery_unknown","queued":true}),
                    );
                    anyhow::bail!(
                        "stdin delivery_unknown: durable preparation succeeded and input was queued, but queue receipt commit failed; do not resend"
                    );
                }
                return Ok(state.executions[id].record.clone());
            } else {
                ensure!(execution.record["io"] == "pty", "resize requires a PTY");
                let rows = args["rows"].as_u64().context("missing rows")?;
                let cols = args["cols"].as_u64().context("missing cols")?;
                ensure!(
                    (1..=1000).contains(&rows) && (1..=1000).contains(&cols),
                    "invalid dimensions"
                );
                Action::Resize(rows as u16, cols as u16)
            };
            permit.send(action);
            Ok(state.executions[id].record.clone())
        }
        "read_file" | "write_file" | "list_directory" | "get_image" => {
            crate::base::tools::call(text(&args, "op")?, &args).await
        }
        "import_file" => crate::files::import(&args).await,
        _ => anyhow::bail!("unknown broker operation"),
    }
}
pub async fn run() -> Result<()> {
    workspace::clean_environment()?;
    let state = workspace::state_dir()?;
    ensure!(state.is_absolute(), "state must be absolute");
    std::fs::create_dir_all(state.join("sessions"))?;
    std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o700))?;
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(state.join("broker.lock"))?;
    ensure!(
        unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0,
        "broker already running"
    );
    let socket = state.join("broker.sock");
    if socket.exists() {
        std::fs::remove_file(&socket)?;
    }
    let listener = UnixListener::bind(&socket)?;
    std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))?;
    std::fs::write(state.join("broker.pid"), std::process::id().to_string())?;
    let ledger = Arc::new(Mutex::new(Ledger::new()?));
    let flushing = ledger.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_millis(250));
        loop {
            interval.tick().await;
            // Broker-owned output batching; never accesses any other service's state.
            let _ = flushing.lock().unwrap().durable.flush();
        }
    });
    // Webhook workers reach the ledger through this broker's own socket.
    let events = crate::events::Events::new(Client::at(socket.clone())).await?;
    let starts = Arc::new(tokio::sync::Mutex::new(()));
    let shutdown = Arc::new(Notify::new());
    let slots = Arc::new(tokio::sync::Semaphore::new(32));
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    loop {
        let stream = tokio::select! {pair=listener.accept()=>pair?.0,_=shutdown.notified()=>break,_=terminate.recv()=>break};
        if stream.peer_cred()?.uid() != unsafe { libc::geteuid() } {
            continue;
        }
        let Ok(slot) = slots.clone().try_acquire_owned() else {
            continue;
        };
        let ledger = ledger.clone();
        let starts = starts.clone();
        let shutdown = shutdown.clone();
        let events = events.clone();
        tokio::spawn(async move {
            let _slot = slot;
            let (reader, mut writer) = stream.into_split();
            let response: Result<Value> = async {
                let mut reader = BufReader::new(reader.take(FRAME + 1));
                let mut bytes = Vec::new();
                tokio::time::timeout(Duration::from_secs(2), reader.read_until(b'\n', &mut bytes))
                    .await??;
                ensure!(
                    bytes.len() <= FRAME as usize && bytes.last() == Some(&b'\n'),
                    "invalid frame"
                );
                let request = serde_json::from_slice(&bytes)?;
                let mut disconnected = [0u8; 1];
                tokio::select! {
                    response=handle(request,ledger,starts,shutdown,events)=>response,
                    _=reader.read(&mut disconnected)=>anyhow::bail!("client disconnected"),
                }
            }
            .await;
            let value = match response {
                Ok(value) => json!({"ok":true,"result":value}),
                Err(error) => {
                    json!({"ok":false,"error":workspace::bounded(&error.to_string(),1024,false).0})
                }
            };
            if let Ok(mut bytes) = serde_json::to_vec(&value) {
                bytes.push(b'\n');
                let _ =
                    tokio::time::timeout(Duration::from_secs(5), writer.write_all(&bytes)).await;
            }
        });
    }
    let actions: Vec<_> = ledger
        .lock()
        .unwrap()
        .executions
        .values()
        .filter(|e| e.record["status"] == "running")
        .map(|e| e.signals.clone())
        .collect();
    for action in actions {
        let _ = action.try_send(libc::SIGKILL);
    }
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let notify = ledger.lock().unwrap().changed.clone();
        let changed = notify.notified();
        tokio::pin!(changed);
        changed.as_mut().enable();
        if ledger
            .lock()
            .unwrap()
            .executions
            .values()
            .all(|e| e.record["status"] != "running")
        {
            break;
        }
        if tokio::time::timeout_at(deadline, changed).await.is_err() {
            eprintln!("Broker shutdown: some child exits remain unconfirmed.");
            break;
        }
    }
    let _ = std::fs::remove_file(socket);
    let _ = std::fs::remove_file(state.join("broker.pid"));
    Ok(())
}
