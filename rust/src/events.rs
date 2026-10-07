//! Webhook delivery owns subscription secrets; it never blocks child pipe readers.
use crate::{broker::Client, workspace};
use anyhow::{Context, Result, ensure};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use subtle::ConstantTimeEq;
use tokio::sync::Mutex as AsyncMutex;
use url::Url;

const NAME: &str = "execution.events";
const MAX_SUBSCRIPTIONS: usize = 8;
const MAX_BODY: usize = 64 * 1024;
const MAX_LEASE: u64 = 24 * 60 * 60;
pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}
fn iso(seconds: u64) -> String {
    time::OffsetDateTime::from_unix_timestamp(seconds as i64)
        .unwrap()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap()
}
pub fn timestamp() -> String {
    iso(now())
}

#[derive(Clone, Serialize, Deserialize)]
struct Subscription {
    id: String,
    owner: u32,
    url: String,
    secret: String,
    session_id: String,
    execution_id: Option<String>,
    expires: u64,
    cursor: Option<String>,
    old_secret: Option<(String, u64)>,
    pending: Option<Pending>,
    suspended: bool,
}
#[derive(Clone, Serialize, Deserialize)]
struct Pending {
    event_id: String,
    body: String,
    cursor: String,
    attempts: u8,
}
#[derive(Clone, Default)]
struct Sender {
    #[cfg(test)]
    allow_loopback: bool,
}
impl Sender {
    async fn post(
        &self,
        subscription: &Subscription,
        event_id: &str,
        body: &str,
    ) -> Result<(u16, Vec<u8>)> {
        ensure!(body.len() <= MAX_BODY, "event exceeds payload bound");
        let url = Url::parse(&subscription.url).map_err(|_| anyhow::anyhow!("invalid_callback"))?;
        #[cfg(test)]
        let testing = self.allow_loopback;
        #[cfg(not(test))]
        let testing = false;
        ensure!(
            (url.scheme() == "https" && url.port_or_known_default() == Some(443))
                || (testing && url.scheme() == "http"),
            "invalid_callback"
        );
        ensure!(
            url.username().is_empty() && url.password().is_none() && url.fragment().is_none(),
            "invalid_callback"
        );
        let host = url.host_str().context("invalid_callback")?;
        let addresses: Vec<_> = tokio::time::timeout(
            Duration::from_secs(5),
            tokio::net::lookup_host((host, url.port_or_known_default().unwrap())),
        )
        .await
        .map_err(|_| anyhow::anyhow!("dns_failed"))?
        .map_err(|_| anyhow::anyhow!("dns_failed"))?
        .collect();
        ensure!(
            !addresses.is_empty()
                && addresses
                    .iter()
                    .all(|a| crate::files::public_address(a.ip())
                        || (testing && a.ip().is_loopback())),
            "private_callback"
        );
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(10))
            .connect_timeout(Duration::from_secs(5))
            .resolve_to_addrs(host, &addresses)
            .build()?;
        let signed = now();
        let mut signatures = vec![signature(&subscription.secret, event_id, signed, body)?];
        if let Some((old, expires)) = &subscription.old_secret {
            if *expires > signed {
                signatures.push(signature(old, event_id, signed, body)?);
            }
        }
        let mut response = client
            .post(url)
            .header("Content-Type", "application/json")
            .header("webhook-id", event_id)
            .header("webhook-timestamp", signed.to_string())
            .header("webhook-signature", signatures.join(" "))
            .header("X-MCP-Subscription-Id", &subscription.id)
            .body(body.to_owned())
            .send()
            .await
            .map_err(|_| anyhow::anyhow!("timeout_or_transport"))?;
        let status = response.status().as_u16();
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| anyhow::anyhow!("response_failed"))?
        {
            ensure!(bytes.len() + chunk.len() <= 4096, "response_too_large");
            bytes.extend_from_slice(&chunk);
        }
        Ok((status, bytes))
    }
}
fn key(secret: &str) -> Result<Vec<u8>> {
    let key = STANDARD
        .decode(
            secret
                .strip_prefix("whsec_")
                .context("invalid signing secret")?,
        )
        .map_err(|_| anyhow::anyhow!("invalid signing secret"))?;
    ensure!(
        (24..=64).contains(&key.len()),
        "invalid signing secret length"
    );
    Ok(key)
}
fn signature(secret: &str, id: &str, at: u64, body: &str) -> Result<String> {
    let mut mac = Hmac::<Sha256>::new_from_slice(&key(secret)?).unwrap();
    mac.update(format!("{id}.{at}.{body}").as_bytes());
    Ok(format!(
        "v1,{}",
        STANDARD.encode(mac.finalize().into_bytes())
    ))
}
#[derive(Clone)]
pub struct Events {
    broker: Client,
    path: PathBuf,
    subscriptions: Arc<AsyncMutex<HashMap<String, Subscription>>>,
    control: Arc<AsyncMutex<()>>,
    workers: Arc<Mutex<HashMap<String, tokio::task::AbortHandle>>>,
    verified: Arc<AsyncMutex<HashMap<String, u64>>>,
    sender: Sender,
}
impl Events {
    pub async fn new(broker: Client) -> Result<Self> {
        Self::load(
            broker,
            workspace::state_dir()?.join("events"),
            Sender::default(),
        )
        .await
    }
    async fn load(broker: Client, directory: PathBuf, sender: Sender) -> Result<Self> {
        std::fs::create_dir_all(&directory)?;
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let metadata = std::fs::symlink_metadata(&directory)?;
        ensure!(
            metadata.is_dir() && metadata.uid() == unsafe { libc::geteuid() },
            "events directory must be owned by this UID"
        );
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))?;
        let path = directory.join("subscriptions.json");
        let mut subscriptions: HashMap<String, Subscription> = if path.exists() {
            let metadata = std::fs::symlink_metadata(&path)?;
            ensure!(
                metadata.is_file()
                    && metadata.uid() == unsafe { libc::geteuid() }
                    && metadata.mode() & 0o077 == 0
                    && metadata.len() <= 1024 * 1024,
                "invalid subscription store permissions or size"
            );
            serde_json::from_slice(&tokio::fs::read(&path).await?)?
        } else {
            HashMap::new()
        };
        subscriptions.retain(|_, s| s.expires > now() && s.owner == unsafe { libc::geteuid() });
        ensure!(
            subscriptions.len() <= MAX_SUBSCRIPTIONS,
            "subscription limit exceeded"
        );
        let engine = Self {
            broker,
            path,
            subscriptions: Arc::new(AsyncMutex::new(subscriptions)),
            control: Arc::new(AsyncMutex::new(())),
            workers: Arc::new(Mutex::new(HashMap::new())),
            verified: Arc::new(AsyncMutex::new(HashMap::new())),
            sender,
        };
        let ids: Vec<_> = engine
            .subscriptions
            .lock()
            .await
            .values()
            .filter(|s| !s.suspended)
            .map(|s| s.id.clone())
            .collect();
        for id in ids {
            engine.launch(id);
        }
        Ok(engine)
    }
    async fn save(&self, values: &HashMap<String, Subscription>) -> Result<()> {
        let bytes = serde_json::to_vec(values)?;
        ensure!(
            bytes.len() <= 1024 * 1024,
            "subscription store exceeds bound"
        );
        let temporary = self.path.with_extension("tmp");
        use std::os::unix::fs::OpenOptionsExt;
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&temporary)?;
        let mut file = tokio::fs::File::from_std(file);
        file.write_all(&bytes).await?;
        file.sync_all().await?;
        tokio::fs::rename(temporary, &self.path).await?;
        std::fs::File::open(self.path.parent().unwrap())?.sync_all()?;
        Ok(())
    }
    pub fn list() -> Value {
        json!({"events":[{"name":NAME,"description":"Batched terminal/pipe output, input receipts, execution state and exit. Data only; recover sequence gaps with read_execution.","delivery":["webhook"],"inputSchema":{"type":"object","properties":{"session_id":{"type":"string"},"execution_id":{"type":"string"}},"required":["session_id"],"additionalProperties":false},"payloadSchema":{"type":"object","properties":{"events":{"type":"array","items":{"type":"object"}},"catch_up_required":{"type":"boolean"},"earliest_cursor":{"type":"string"}},"required":["events","catch_up_required","earliest_cursor"],"additionalProperties":false}}]})
    }
    async fn identity(&self, params: &Value) -> Result<(String, String, String, Option<String>)> {
        ensure!(params["name"] == NAME, "unknown event");
        ensure!(
            params["delivery"]["mode"] == "webhook",
            "only webhook delivery is supported"
        );
        let arguments = params["arguments"]
            .as_object()
            .context("missing event arguments")?;
        ensure!(
            arguments
                .keys()
                .all(|k| ["session_id", "execution_id"].contains(&k.as_str())),
            "unknown event filter"
        );
        let session = workspace::text(&params["arguments"], "session_id")?.to_owned();
        self.broker
            .call(json!({"op":"inspect_session","session_id":session}))
            .await?;
        let execution = params["arguments"]
            .get("execution_id")
            .map(|v| v.as_str().context("invalid execution_id"))
            .transpose()?
            .map(str::to_owned);
        if let Some(id) = &execution {
            let data = self
                .broker
                .call(json!({"op":"read_execution","execution_id":id}))
                .await?;
            ensure!(
                data["execution"]["session_id"] == session,
                "execution does not belong to this session"
            );
        }
        let url = workspace::text(&params["delivery"], "url")?.to_owned();
        ensure!(url.len() <= 8192, "callback URL too long");
        let identity = serde_json::to_vec(&json!([
            unsafe { libc::geteuid() },
            url,
            NAME,
            session,
            execution
        ]))?;
        let id = format!("sub_{:x}", Sha256::digest(identity));
        Ok((id, url, session, execution))
    }
    pub async fn subscribe(&self, params: Value) -> Result<Value> {
        let _guard = self.control.lock().await;
        let (id, url, session_id, execution_id) = self.identity(&params).await?;
        let secret = workspace::text(&params["delivery"], "secret")?.to_owned();
        key(&secret)?;
        let ttl = match params.get("ttlMs") {
            None | Some(Value::Null) => MAX_LEASE,
            Some(v) => v
                .as_u64()
                .context("invalid ttlMs")?
                .div_ceil(1000)
                .clamp(1, MAX_LEASE),
        };
        let existing = {
            let store = self.subscriptions.lock().await;
            ensure!(
                store.contains_key(&id)
                    || store.values().filter(|s| s.expires > now()).count() < MAX_SUBSCRIPTIONS,
                "at most 8 subscriptions"
            );
            store.get(&id).cloned()
        };
        let prior = existing
            .as_ref()
            .map(|s| s.secret.clone())
            .filter(|key| key != &secret)
            .map(|key| (key, now() + 300));
        let mut subscription = Subscription {
            id: id.clone(),
            owner: unsafe { libc::geteuid() },
            url,
            secret,
            session_id,
            execution_id,
            expires: now() + ttl,
            cursor: existing.as_ref().and_then(|s| s.cursor.clone()),
            old_secret: prior.or_else(|| existing.as_ref().and_then(|s| s.old_secret.clone())),
            pending: existing.as_ref().and_then(|s| s.pending.clone()),
            suspended: false,
        };
        let cache_key = format!("{}:{}", subscription.owner, subscription.url);
        let verified = self
            .verified
            .lock()
            .await
            .get(&cache_key)
            .is_some_and(|expiry| *expiry > now());
        if !verified {
            let challenge = uuid::Uuid::new_v4().simple().to_string();
            let verification_id = format!("verification_{}", uuid::Uuid::new_v4().simple());
            let body =
                serde_json::to_string(&json!({"type":"verification","challenge":challenge}))?;
            let (status, bytes) = self
                .sender
                .post(&subscription, &verification_id, &body)
                .await?;
            let response: Value =
                serde_json::from_slice(&bytes).map_err(|_| anyhow::anyhow!("challenge_failed"))?;
            let echoed = response["challenge"].as_str().unwrap_or("");
            ensure!(
                (200..300).contains(&status)
                    && bool::from(
                        Sha256::digest(echoed.as_bytes())
                            .ct_eq(&Sha256::digest(challenge.as_bytes()))
                    ),
                "challenge_failed"
            );
            let mut cache = self.verified.lock().await;
            cache.retain(|_, expiry| *expiry > now());
            if cache.len() >= MAX_SUBSCRIPTIONS {
                cache.clear();
            }
            cache.insert(cache_key, now() + 300);
        }
        if let Some(worker) = self.workers.lock().unwrap().remove(&id) {
            worker.abort();
        }
        let mut store = self.subscriptions.lock().await;
        store.retain(|_, s| s.expires > now());
        if let Some(latest) = store.get(&id) {
            subscription.cursor = latest.cursor.clone();
            subscription.pending = latest.pending.clone();
        }
        // Refresh preserves the serialized unacknowledged event even if an
        // earlier delivery completed while callback verification was in flight.
        if let Some(pending) = subscription.pending.as_mut() {
            pending.attempts = 0;
        }
        if let Some(cursor) = params.get("cursor").and_then(Value::as_str) {
            if subscription.pending.is_none() {
                subscription.cursor = Some(cursor.to_owned());
            }
        }
        let mut read = json!({"op":"event_position","session_id":subscription.session_id,"execution_id":subscription.execution_id,"cursor":subscription.cursor});
        if subscription.execution_id.is_some() {
            read["op"] = json!("read_execution");
        }
        let position = self.broker.call(read).await?;
        let truncated = position["catch_up_required"] == true;
        if subscription.cursor.is_none() {
            subscription.cursor = position["cursor"].as_str().map(str::to_owned);
        }
        let result = json!({"id":id,"refreshBefore":iso(subscription.expires),"cursor":subscription.cursor,"truncated":truncated});
        {
            let mut workers = self.workers.lock().unwrap();
            workers.retain(|key, worker| {
                if !store.contains_key(key) {
                    worker.abort();
                    return false;
                }
                !worker.is_finished()
            });
        }
        store.insert(id.clone(), subscription);
        self.save(&store).await?;
        drop(store);
        self.launch(id);
        Ok(result)
    }
    pub async fn unsubscribe(&self, params: Value) -> Result<Value> {
        let _guard = self.control.lock().await;
        // Session close revokes access but must not prevent idempotent cleanup.
        ensure!(
            params["name"] == NAME && params["delivery"]["mode"] == "webhook",
            "invalid unsubscribe"
        );
        let session = workspace::text(&params["arguments"], "session_id")?;
        let execution = params["arguments"]
            .get("execution_id")
            .and_then(Value::as_str);
        let url = workspace::text(&params["delivery"], "url")?;
        let id = format!(
            "sub_{:x}",
            Sha256::digest(serde_json::to_vec(&json!([
                unsafe { libc::geteuid() },
                url,
                NAME,
                session,
                execution
            ]))?)
        );
        if let Some(worker) = self.workers.lock().unwrap().remove(&id) {
            worker.abort();
        }
        let mut store = self.subscriptions.lock().await;
        store.remove(&id);
        self.save(&store).await?;
        Ok(json!({}))
    }
    fn launch(&self, id: String) {
        let engine = self.clone();
        let key = id.clone();
        let task = tokio::spawn(async move {
            if engine.deliver(&id).await.is_err() {
                let mut store = engine.subscriptions.lock().await;
                if let Some(s) = store.get_mut(&id) {
                    s.suspended = true;
                    let _ = engine.save(&store).await;
                }
                eprintln!("Events subscription suspended; refresh or inspect execution history.");
            }
        });
        let mut workers = self.workers.lock().unwrap();
        workers.retain(|_, worker| !worker.is_finished());
        workers.insert(key, task.abort_handle());
    }
    async fn deliver(&self, id: &str) -> Result<()> {
        loop {
            let subscription = match self.subscriptions.lock().await.get(id).cloned() {
                Some(s) => s,
                None => return Ok(()),
            };
            if subscription.expires <= now() {
                let mut store = self.subscriptions.lock().await;
                store.remove(id);
                self.save(&store).await?;
                return Ok(());
            }
            self.broker
                .call(json!({"op":"inspect_session","session_id":subscription.session_id}))
                .await?;
            if subscription.pending.is_none() {
                let request = json!({"op":"wait_events","session_id":subscription.session_id,"execution_id":subscription.execution_id,"cursor":subscription.cursor});
                let batch = tokio::time::timeout(
                    Duration::from_secs(subscription.expires.saturating_sub(now()).max(1)),
                    self.broker.call(request),
                )
                .await;
                let batch = match batch {
                    Ok(batch) => batch?,
                    Err(_) => continue,
                };
                // Coalesce the first wake with subsequent output without holding the pipe reader.
                tokio::time::sleep(Duration::from_millis(100)).await;
                let mut read = json!({"op":"event_position","session_id":subscription.session_id,"execution_id":subscription.execution_id,"cursor":subscription.cursor});
                if subscription.execution_id.is_some() {
                    read["op"] = json!("read_execution");
                }
                let batch = self.broker.call(read).await.unwrap_or(batch);
                let cursor = workspace::text(&batch, "cursor")?.to_owned();
                let event_id = format!(
                    "evt_{:x}",
                    Sha256::digest(format!("{id}:{cursor}").as_bytes())
                );
                let occurrence = batch["events"]
                    .as_array()
                    .and_then(|a| a.last())
                    .and_then(|v| v["timestamp"].as_str())
                    .map(str::to_owned)
                    .unwrap_or_else(timestamp);
                let body = serde_json::to_string(
                    &json!({"eventId":event_id,"name":NAME,"timestamp":occurrence,"data":{"events":batch["events"],"catch_up_required":batch["catch_up_required"],"earliest_cursor":batch["earliest_cursor"]},"cursor":cursor}),
                )?;
                ensure!(body.len() <= MAX_BODY, "event exceeds bounded payload");
                let mut store = self.subscriptions.lock().await;
                let Some(s) = store.get_mut(id) else {
                    return Ok(());
                };
                s.pending = Some(Pending {
                    event_id,
                    body,
                    cursor,
                    attempts: 0,
                });
                self.save(&store).await?;
                continue;
            }
            let pending = subscription.pending.as_ref().unwrap();
            if pending.attempts >= 5 {
                anyhow::bail!("delivery attempts exhausted; pending cursor retained")
            }
            let response = self
                .sender
                .post(&subscription, &pending.event_id, &pending.body)
                .await;
            let status = response.as_ref().ok().map(|(status, _)| *status);
            let mut store = self.subscriptions.lock().await;
            let Some(s) = store.get_mut(id) else {
                return Ok(());
            };
            if status.is_some_and(|s| (200..300).contains(&s)) {
                s.cursor = Some(pending.cursor.clone());
                s.pending = None;
                self.save(&store).await?;
            } else if matches!(status, Some(410 | 413)) {
                store.remove(id);
                self.save(&store).await?;
                return Ok(());
            } else {
                if status.is_some_and(|s| s < 500 && s != 429) {
                    s.suspended = true;
                    self.save(&store).await?;
                    return Ok(());
                }
                let attempts = s.pending.as_mut().unwrap();
                attempts.attempts += 1;
                let backoff = 1u64 << attempts.attempts.min(4);
                self.save(&store).await?;
                drop(store);
                tokio::time::sleep(Duration::from_secs(backoff)).await;
            }
        }
    }
}
use tokio::io::AsyncWriteExt;

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

    #[tokio::test]
    async fn signed_webhooks_preserve_exact_retry_body_and_support_key_rotation() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let secret = format!("whsec_{}", STANDARD.encode([7; 32]));
        let old = format!("whsec_{}", STANDARD.encode([8; 32]));
        let body=serde_json::to_string(&json!({"eventId":"stable","name":NAME,"timestamp":timestamp(),"data":{"events":[{"text":"日本語\u{0}"}],"catch_up_required":false,"earliest_cursor":"epoch:0"},"cursor":"epoch:1"})).unwrap();
        let expected = body.clone();
        let new_key = secret.clone();
        let old_key = old.clone();
        let receiver = tokio::spawn(async move {
            for attempt in 0..2 {
                let (socket, _) = listener.accept().await.unwrap();
                let mut reader = BufReader::new(socket);
                let mut line = String::new();
                reader.read_line(&mut line).await.unwrap();
                assert!(line.starts_with("POST /callback "));
                let mut headers = HashMap::new();
                loop {
                    line.clear();
                    reader.read_line(&mut line).await.unwrap();
                    if line == "\r\n" {
                        break;
                    }
                    let (name, value) = line.split_once(':').unwrap();
                    headers.insert(name.to_ascii_lowercase(), value.trim().to_owned());
                }
                let length: usize = headers["content-length"].parse().unwrap();
                let mut received = vec![0; length];
                reader.read_exact(&mut received).await.unwrap();
                assert_eq!(received, expected.as_bytes());
                assert_eq!(headers["webhook-id"], "stable");
                assert_eq!(headers["x-mcp-subscription-id"], "sub_test");
                let at = headers["webhook-timestamp"].parse().unwrap();
                assert_eq!(
                    headers["webhook-signature"],
                    format!(
                        "{} {}",
                        signature(&new_key, "stable", at, &expected).unwrap(),
                        signature(&old_key, "stable", at, &expected).unwrap()
                    )
                );
                let status = if attempt == 0 {
                    "503 Service Unavailable"
                } else {
                    "204 No Content"
                };
                reader
                    .get_mut()
                    .write_all(
                        format!(
                            "HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                        )
                        .as_bytes(),
                    )
                    .await
                    .unwrap();
            }
        });
        let subscription = Subscription {
            id: "sub_test".into(),
            owner: unsafe { libc::geteuid() },
            url: format!("http://{address}/callback"),
            secret,
            session_id: "project".into(),
            execution_id: None,
            expires: now() + 60,
            cursor: None,
            old_secret: Some((old, now() + 60)),
            pending: None,
            suspended: false,
        };
        let sender = Sender {
            allow_loopback: true,
        };
        assert_eq!(
            sender.post(&subscription, "stable", &body).await.unwrap().0,
            503
        );
        assert_eq!(
            sender.post(&subscription, "stable", &body).await.unwrap().0,
            204
        );
        receiver.await.unwrap();
        assert_eq!(
            Sender::default()
                .post(&subscription, "stable", &body)
                .await
                .unwrap_err()
                .to_string(),
            "invalid_callback"
        );
        let mut private = subscription;
        private.url = "https://127.0.0.1/callback".into();
        assert_eq!(
            Sender::default()
                .post(&private, "stable", &body)
                .await
                .unwrap_err()
                .to_string(),
            "private_callback"
        );
    }
    #[tokio::test]
    async fn real_broker_push_verification_persistence_and_lease_expiry() -> Result<()> {
        let root = tempfile::tempdir()?;
        let binary = std::env::current_exe()?
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("dev-session-mcp");
        ensure!(
            binary.is_file(),
            "run cargo build before cargo test for the real broker check"
        );
        let state = root.path().join("broker");
        let project = root.path().join("project");
        std::fs::create_dir(&project)?;
        let mut process = tokio::process::Command::new(binary)
            .arg("broker")
            .env_clear()
            .envs(workspace::safe_environment())
            .env("DEV_SESSION_MCP_STATE_DIR", &state)
            .kill_on_drop(true)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()?;
        let broker = Client::test_socket(state.join("broker.sock"));
        for _ in 0..100 {
            if broker.call(json!({"op":"ping"})).await.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let session = broker
            .call(json!({"op":"open_session","cwd":project}))
            .await?["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let url = format!("http://{}/callback", listener.local_addr()?);
        let (received, mut requests) = tokio::sync::mpsc::channel(8);
        let acknowledge = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let ack = acknowledge.clone();
        let receiver = tokio::spawn(async move {
            loop {
                let (socket, _) = listener.accept().await.unwrap();
                let mut reader = BufReader::new(socket);
                let mut line = String::new();
                reader.read_line(&mut line).await.unwrap();
                let mut headers = HashMap::new();
                loop {
                    line.clear();
                    reader.read_line(&mut line).await.unwrap();
                    if line == "\r\n" {
                        break;
                    }
                    let (key, value) = line.split_once(':').unwrap();
                    headers.insert(key.to_ascii_lowercase(), value.trim().to_owned());
                }
                let length: usize = headers["content-length"].parse().unwrap();
                let mut body = vec![0; length];
                reader.read_exact(&mut body).await.unwrap();
                let value: Value = serde_json::from_slice(&body).unwrap();
                let (status, reply) = if value["type"] == "verification" {
                    (
                        "200 OK",
                        serde_json::to_string(&json!({"challenge":value["challenge"]})).unwrap(),
                    )
                } else if ack.load(std::sync::atomic::Ordering::SeqCst) {
                    ("204 No Content", String::new())
                } else {
                    ("503 Service Unavailable", String::new())
                };
                reader.get_mut().write_all(format!("HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",reply.len()).as_bytes()).await.unwrap();
                received.send((headers, body)).await.unwrap();
            }
        });
        let directory = root.path().join("events");
        let engine = Events::load(
            broker.clone(),
            directory.clone(),
            Sender {
                allow_loopback: true,
            },
        )
        .await?;
        let secret = format!("whsec_{}", STANDARD.encode([9; 32]));
        let subscription = json!({"name":NAME,"arguments":{"session_id":session},"delivery":{"mode":"webhook","url":url,"secret":secret},"ttlMs":30000});
        let granted = engine.subscribe(subscription.clone()).await?;
        let id = granted["id"].as_str().unwrap().to_owned();
        let (headers, verification) = tokio::time::timeout(Duration::from_secs(5), requests.recv())
            .await?
            .unwrap();
        let verification: Value = serde_json::from_slice(&verification)?;
        ensure!(verification["type"] == "verification");
        ensure!(headers["webhook-id"].starts_with("verification_"));
        broker.call(json!({"op":"start_execution","session_id":session,"profile":"host","io":"pipes","command":["/bin/sh","-c","printf actual-push"]})).await?;
        let (_, first) = tokio::time::timeout(Duration::from_secs(5), requests.recv())
            .await?
            .unwrap();
        let value: Value = serde_json::from_slice(&first)?;
        ensure!(
            value["data"]["events"]
                .as_array()
                .unwrap()
                .iter()
                .any(|e| e["data"]["text"] == "actual-push")
        );
        for _ in 0..100 {
            if engine.subscriptions.lock().await[&id]
                .pending
                .as_ref()
                .is_some_and(|p| p.attempts == 1)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        for worker in engine.workers.lock().unwrap().values() {
            worker.abort();
        }
        acknowledge.store(true, std::sync::atomic::Ordering::SeqCst);
        let restored = Events::load(
            broker.clone(),
            directory.clone(),
            Sender {
                allow_loopback: true,
            },
        )
        .await?;
        let (_, retry) = tokio::time::timeout(Duration::from_secs(5), requests.recv())
            .await?
            .unwrap();
        ensure!(
            first == retry,
            "restart must preserve the exact pending body and event ID"
        );
        for _ in 0..100 {
            if restored.subscriptions.lock().await[&id].pending.is_none() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let mut renewed = subscription.clone();
        renewed["ttlMs"] = json!(1000);
        ensure!(restored.subscribe(renewed).await?["id"] == id);
        // Verification cache is deliberately in-memory; a frontend restart verifies again.
        let (_, fresh) = tokio::time::timeout(Duration::from_secs(5), requests.recv())
            .await?
            .unwrap();
        ensure!(serde_json::from_slice::<Value>(&fresh)?["type"] == "verification");
        tokio::time::sleep(Duration::from_millis(1200)).await;
        ensure!(
            !restored.subscriptions.lock().await.contains_key(&id),
            "expired lease must stop its waiting worker"
        );
        restored.unsubscribe(subscription.clone()).await?;
        restored.unsubscribe(subscription).await?;
        use std::os::unix::fs::PermissionsExt;
        ensure!(
            std::fs::metadata(directory.join("subscriptions.json"))?
                .permissions()
                .mode()
                & 0o777
                == 0o600
        );
        receiver.abort();
        broker.call(json!({"op":"shutdown"})).await?;
        tokio::time::timeout(Duration::from_secs(5), process.wait()).await??;
        Ok(())
    }
}
