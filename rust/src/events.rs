//! Webhook delivery owns subscription secrets; it never blocks child pipe readers.
use crate::{broker::Client, workspace};
use anyhow::{Context, Result, ensure};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, VecDeque},
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
const DELIVERY_HISTORY: usize = 16;
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
    timestamp_ms(millis())
}
fn timestamp_ms(at_ms: u64) -> String {
    time::OffsetDateTime::from_unix_timestamp_nanos(i128::from(at_ms) * 1_000_000)
        .unwrap()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap()
}
fn millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
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
    #[serde(default)]
    delivery_history: VecDeque<DeliveryAttempt>,
}
#[derive(Clone, Serialize, Deserialize)]
struct DeliveryAttempt {
    event_id: String,
    cursor: String,
    queued_at_ms: Option<u64>,
    first_event_at: Option<String>,
    last_event_at: Option<String>,
    attempt: u8,
    started_at_ms: u64,
    finished_at_ms: u64,
    elapsed_ms: u64,
    status: Option<u16>,
}
#[derive(Clone, Serialize, Deserialize)]
struct Pending {
    event_id: String,
    body: String,
    cursor: String,
    attempts: u8,
    #[serde(default)]
    queued_at_ms: Option<u64>,
}
impl Subscription {
    fn diagnostic(&self, observed_at_ms: u64, worker_running: bool) -> Value {
        let expired = self.expires <= observed_at_ms / 1000;
        let state = if expired {
            "expired"
        } else if self.suspended {
            "suspended"
        } else if !worker_running {
            "worker_stopped"
        } else {
            "active"
        };
        // Explicit allowlist: never serialize the subscription or pending body.
        let history: Vec<_> = self
            .delivery_history
            .iter()
            .rev()
            .take(DELIVERY_HISTORY)
            .rev()
            .map(|a| {
                json!({"event_id":a.event_id,"attempt":a.attempt,
                "queued_at_ms":a.queued_at_ms,"first_event_at":a.first_event_at,
                "last_event_at":a.last_event_at,"started_at_ms":a.started_at_ms,
                "finished_at_ms":a.finished_at_ms,"elapsed_ms":a.elapsed_ms,
                "status":a.status})
            })
            .collect();
        json!({"subscription_id":self.id,"execution_id":self.execution_id,
            "lease_expires_at_unix_seconds":self.expires,"expired":expired,
            "suspended":self.suspended,"worker_running":worker_running,"delivery_state":state,
            "has_unacknowledged_batch":self.pending.is_some(),
            "pending_queued_at_ms":self.pending.as_ref().and_then(|p| p.queued_at_ms),
            "last_http_status":self.delivery_history.back().and_then(|a| a.status),
            "delivery_history":history})
    }
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
        self.request(subscription, event_id, body, false).await
    }
    async fn verify(
        &self,
        subscription: &Subscription,
        event_id: &str,
        body: &str,
    ) -> Result<(u16, Vec<u8>)> {
        self.request(subscription, event_id, body, true).await
    }
    async fn request(
        &self,
        subscription: &Subscription,
        event_id: &str,
        body: &str,
        read_response_body: bool,
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
        // Event receipt is the HTTP status. Waiting for an optional response body
        // can turn an accepted event into a timeout and hold the whole outbox.
        // Callback verification still needs its bounded challenge response.
        if !read_response_body {
            return Ok((status, Vec::new()));
        }
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
    #[cfg(test)]
    pub(crate) fn empty_for_protocol_test(broker: Client, path: PathBuf) -> Self {
        Self {
            broker,
            path,
            subscriptions: Arc::new(AsyncMutex::new(HashMap::new())),
            control: Arc::new(AsyncMutex::new(())),
            workers: Arc::new(Mutex::new(HashMap::new())),
            verified: Arc::new(AsyncMutex::new(HashMap::new())),
            sender: Sender::default(),
        }
    }
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
        for subscription in subscriptions.values_mut() {
            while subscription.delivery_history.len() > DELIVERY_HISTORY {
                subscription.delivery_history.pop_front();
            }
        }
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
    /// Snapshot only: never load secrets, refresh a lease, or launch a worker.
    pub async fn diagnostics(&self, params: Value) -> Result<Value> {
        self.diagnostics_with(params, |request| self.broker.call(request))
            .await
    }
    async fn diagnostics_with<F, Fut>(&self, params: Value, read: F) -> Result<Value>
    where
        F: Fn(Value) -> Fut,
        Fut: std::future::Future<Output = Result<Value>>,
    {
        let args = params
            .as_object()
            .context("expected diagnostic arguments")?;
        ensure!(
            args.keys()
                .all(|k| ["session_id", "execution_id"].contains(&k.as_str())),
            "unknown diagnostic argument"
        );
        let session = workspace::text(&params, "session_id")?;
        let execution = params
            .get("execution_id")
            .map(|_| workspace::text(&params, "execution_id"))
            .transpose()?;
        // Use the same session/retained-history and execution ownership checks
        // as subscription creation. Do not expose broker errors or record bodies.
        let info = read(json!({"op":"inspect_event_session","session_id":session}))
            .await
            .map_err(|_| anyhow::anyhow!("session diagnostics unavailable"))?;
        let execution_state = if let Some(id) = execution {
            let data = read(json!({"op":"read_execution","execution_id":id}))
                .await
                .map_err(|_| anyhow::anyhow!("execution diagnostics unavailable"))?;
            ensure!(
                data["execution"]["session_id"] == session,
                "execution does not belong to this session"
            );
            json!({"execution_id":id,"status":data["execution"]["status"],
                "exit_code":data["execution"]["exit_code"]})
        } else {
            Value::Null
        };
        let store = self.subscriptions.lock().await;
        let observed_at_ms = millis();
        let workers = self.workers.lock().unwrap();
        let mut subscriptions: Vec<_> = store
            .values()
            .filter(|s| {
                s.owner == unsafe { libc::geteuid() }
                    && s.session_id == session
                    && execution.is_none_or(|id| {
                        s.execution_id.as_deref().is_none_or(|filter| filter == id)
                    })
            })
            .map(|s| {
                let worker_running = workers.get(&s.id).is_some_and(|w| !w.is_finished());
                s.diagnostic(observed_at_ms, worker_running)
            })
            .collect();
        subscriptions.sort_by(|a, b| {
            a["subscription_id"]
                .as_str()
                .cmp(&b["subscription_id"].as_str())
        });
        Ok(json!({"observed_at_ms":observed_at_ms,"session_id":session,
            "session_state":info["state"],"execution":execution_state,
            "subscriptions":subscriptions}))
    }
    async fn identity(
        &self,
        params: &Value,
    ) -> Result<(String, String, String, Option<String>, bool)> {
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
        let session_info = self
            .broker
            .call(json!({"op":"inspect_event_session","session_id":session}))
            .await?;
        let closed = session_info["state"] == "closed";
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
        if closed {
            ensure!(
                self.subscriptions
                    .lock()
                    .await
                    .get(&id)
                    .is_some_and(|s| s.owner == unsafe { libc::geteuid() } && s.expires > now()),
                "closed session only retains its existing unexpired subscription"
            );
        }
        Ok((id, url, session, execution, closed))
    }
    pub async fn subscribe(&self, params: Value) -> Result<Value> {
        let _guard = self.control.lock().await;
        let (id, url, session_id, execution_id, closed) = self.identity(&params).await?;
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
            expires: if closed {
                existing.as_ref().unwrap().expires.min(now() + ttl)
            } else {
                now() + ttl
            },
            cursor: existing.as_ref().and_then(|s| s.cursor.clone()),
            old_secret: prior.or_else(|| existing.as_ref().and_then(|s| s.old_secret.clone())),
            pending: existing.as_ref().and_then(|s| s.pending.clone()),
            suspended: false,
            delivery_history: existing
                .as_ref()
                .map(|s| s.delivery_history.clone())
                .unwrap_or_default(),
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
                .verify(&subscription, &verification_id, &body)
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
        ensure!(
            subscription.expires > now(),
            "subscription expired during verification"
        );
        if let Some(worker) = self.workers.lock().unwrap().remove(&id) {
            worker.abort();
        }
        let mut store = self.subscriptions.lock().await;
        store.retain(|_, s| s.expires > now());
        if let Some(latest) = store.get(&id) {
            subscription.cursor = latest.cursor.clone();
            subscription.pending = latest.pending.clone();
            subscription.delivery_history = latest.delivery_history.clone();
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
                .call(json!({"op":"inspect_event_session","session_id":subscription.session_id}))
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
                    queued_at_ms: Some(millis()),
                });
                self.save(&store).await?;
                continue;
            }
            let pending = subscription.pending.as_ref().unwrap();
            if pending.attempts >= 5 {
                anyhow::bail!("delivery attempts exhausted; pending cursor retained")
            }
            let started_at_ms = millis();
            let started = tokio::time::Instant::now();
            let response = self
                .sender
                .post(&subscription, &pending.event_id, &pending.body)
                .await;
            let finished_at_ms = millis();
            let elapsed_ms = started.elapsed().as_millis() as u64;
            let status = response.as_ref().ok().map(|(status, _)| *status);
            let mut store = self.subscriptions.lock().await;
            let Some(s) = store.get_mut(id) else {
                return Ok(());
            };
            let body: Value = serde_json::from_str(&pending.body)?;
            let events = body["data"]["events"].as_array();
            s.delivery_history.push_back(DeliveryAttempt {
                event_id: pending.event_id.clone(),
                cursor: pending.cursor.clone(),
                queued_at_ms: pending.queued_at_ms,
                first_event_at: events
                    .and_then(|v| v.first())
                    .and_then(|v| v["timestamp"].as_str())
                    .map(str::to_owned),
                last_event_at: events
                    .and_then(|v| v.last())
                    .and_then(|v| v["timestamp"].as_str())
                    .map(str::to_owned),
                attempt: pending.attempts.saturating_add(1),
                started_at_ms,
                finished_at_ms,
                elapsed_ms,
                status,
            });
            while s.delivery_history.len() > DELIVERY_HISTORY {
                s.delivery_history.pop_front();
            }
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

    fn diagnostic_fixture() -> Subscription {
        Subscription {
            id: "sub_fixture".into(),
            owner: unsafe { libc::geteuid() },
            url: "https://callback.invalid/PRIVATE_URL".into(),
            secret: "PRIVATE_KEY".into(),
            old_secret: Some(("PRIVATE_OLD_KEY".into(), 999)),
            session_id: "session".into(),
            execution_id: None,
            expires: 200,
            cursor: Some("PRIVATE_CURSOR".into()),
            suspended: false,
            pending: Some(Pending {
                event_id: "evt".into(),
                body: "PRIVATE_BODY".into(),
                cursor: "PRIVATE_PENDING_CURSOR".into(),
                attempts: 1,
                queued_at_ms: Some(100_000),
            }),
            delivery_history: VecDeque::from([DeliveryAttempt {
                event_id: "evt".into(),
                cursor: "PRIVATE_HISTORY_CURSOR".into(),
                queued_at_ms: Some(100_000),
                first_event_at: None,
                last_event_at: None,
                attempt: 1,
                started_at_ms: 100_100,
                finished_at_ms: 100_440,
                elapsed_ms: 340,
                status: Some(503),
            }]),
        }
    }

    #[test]
    fn diagnostics_event_timestamps_preserve_milliseconds() {
        assert_eq!(timestamp_ms(1_000), "1970-01-01T00:00:01Z");
        assert_eq!(timestamp_ms(1_123), "1970-01-01T00:00:01.123Z");
        assert_eq!(timestamp_ms(1_999), "1970-01-01T00:00:01.999Z");
    }

    #[test]
    fn diagnostics_states_redaction_and_legacy_history() -> Result<()> {
        let mut s = diagnostic_fixture();
        let value = s.diagnostic(101_000, true);
        assert_eq!(value["delivery_state"], "active");
        assert_eq!(value["last_http_status"], 503);
        assert_eq!(value["has_unacknowledged_batch"], true);
        assert_eq!(value["lease_expires_at_unix_seconds"], 200);
        assert_eq!(value["delivery_history"][0]["elapsed_ms"], 340);
        assert_eq!(value["delivery_history"][0]["started_at_ms"], 100_100);
        assert_eq!(value["delivery_history"][0]["finished_at_ms"], 100_440);
        assert!(!value.to_string().contains("PRIVATE"));
        assert_eq!(
            s.diagnostic(101_000, false)["delivery_state"],
            "worker_stopped"
        );
        s.suspended = true;
        assert_eq!(s.diagnostic(101_000, true)["delivery_state"], "suspended");
        assert_eq!(s.diagnostic(200_000, true)["delivery_state"], "expired");
        s.delivery_history.back_mut().unwrap().status = None;
        assert!(s.diagnostic(101_000, true)["last_http_status"].is_null());
        for _ in 0..20 {
            s.delivery_history.push_back(s.delivery_history[0].clone());
        }
        assert_eq!(
            s.diagnostic(101_000, true)["delivery_history"]
                .as_array()
                .unwrap()
                .len(),
            16
        );
        let mut legacy = serde_json::to_value(&s)?;
        legacy.as_object_mut().unwrap().remove("delivery_history");
        legacy["pending"]
            .as_object_mut()
            .unwrap()
            .remove("queued_at_ms");
        let legacy: Subscription = serde_json::from_value(legacy)?;
        let value = legacy.diagnostic(101_000, false);
        assert_eq!(value["delivery_history"], json!([]));
        assert!(value["last_http_status"].is_null());
        assert!(value["pending_queued_at_ms"].is_null());
        s.pending = None;
        assert_eq!(
            s.diagnostic(101_000, false)["has_unacknowledged_batch"],
            false
        );
        Ok(())
    }

    #[tokio::test]
    async fn diagnostics_read_only_filters_and_authorization() -> Result<()> {
        let root = tempfile::tempdir()?;
        let socket = root.path().join("unused-broker.sock");
        // Exercise the production diagnostic path with synthetic read responses.
        // No socket, callback, command execution or live secret store is used.
        async fn read(request: Value) -> Result<Value> {
            Ok(match request["op"].as_str().unwrap() {
                "inspect_event_session" if request["session_id"] == "missing" => {
                    anyhow::bail!("PRIVATE_BROKER_ERROR")
                }
                "inspect_event_session" => {
                    json!({"state":if request["session_id"] == "closed" { "closed" } else { "open" },"private":"PRIVATE_SESSION"})
                }
                "read_execution" => json!({"execution":{
                    "session_id":match request["execution_id"].as_str() { Some("wrong") => "other", Some("closed") => "closed", _ => "session" },
                    "status":if request["execution_id"] == "closed" { "exited" } else { "running" },
                    "command":["PRIVATE_COMMAND"],"text":"PRIVATE_OUTPUT"
                }}),
                _ => panic!("diagnostic performed a non-read operation"),
            })
        }
        let path = root.path().join("must-not-be-read-or-written");
        tokio::fs::write(&path, b"unchanged sentinel").await?;
        let mut s = diagnostic_fixture();
        s.expires = now() + 60;
        s.suspended = true;
        let mut foreign = s.clone();
        foreign.id = "foreign".into();
        foreign.owner ^= 1;
        let mut other = s.clone();
        other.id = "other".into();
        other.session_id = "other".into();
        let mut filtered = s.clone();
        filtered.id = "filtered".into();
        filtered.execution_id = Some("different".into());
        let engine = Events {
            broker: Client::test_socket(socket),
            path: path.clone(),
            subscriptions: Arc::new(AsyncMutex::new(
                [s, foreign, other, filtered]
                    .into_iter()
                    .map(|s| (s.id.clone(), s))
                    .collect(),
            )),
            control: Arc::new(AsyncMutex::new(())),
            workers: Arc::new(Mutex::new(HashMap::new())),
            verified: Arc::new(AsyncMutex::new(HashMap::new())),
            sender: Sender::default(),
        };
        let before = serde_json::to_value(&*engine.subscriptions.lock().await)?;
        for _ in 0..2 {
            let value = engine
                .diagnostics_with(
                    json!({"session_id":"session","execution_id":"execution"}),
                    read,
                )
                .await?;
            assert_eq!(value["execution"]["status"], "running");
            assert_eq!(value["subscriptions"].as_array().unwrap().len(), 1);
            assert_eq!(value["subscriptions"][0]["delivery_state"], "suspended");
            assert!(!value.to_string().contains("PRIVATE"));
        }
        let all = engine
            .diagnostics_with(json!({"session_id":"session"}), read)
            .await?;
        assert!(all["execution"].is_null());
        assert_eq!(all["subscriptions"].as_array().unwrap().len(), 2);
        let closed = engine
            .diagnostics_with(json!({"session_id":"closed","execution_id":"closed"}), read)
            .await?;
        assert_eq!(closed["session_state"], "closed");
        assert_eq!(closed["execution"]["status"], "exited");
        assert_eq!(
            engine
                .diagnostics_with(json!({"session_id":"empty"}), read)
                .await?["subscriptions"],
            json!([])
        );
        assert!(
            engine
                .diagnostics_with(json!({"session_id":"session","execution_id":"wrong"}), read)
                .await
                .is_err()
        );
        let error = engine
            .diagnostics_with(json!({"session_id":"missing"}), read)
            .await
            .unwrap_err();
        assert_eq!(error.to_string(), "session diagnostics unavailable");
        for invalid in [
            json!({}),
            json!({"session_id":"session","owner":0}),
            json!({"session_id":"session","execution_id":null}),
        ] {
            assert!(engine.diagnostics_with(invalid, read).await.is_err());
        }
        assert_eq!(
            serde_json::to_value(&*engine.subscriptions.lock().await)?,
            before
        );
        assert_eq!(tokio::fs::read(&path).await?, b"unchanged sentinel");
        assert!(engine.workers.lock().unwrap().is_empty());
        assert!(engine.verified.lock().await.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn accepted_event_does_not_wait_for_callback_response_body() -> Result<()> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let (release, hold) = tokio::sync::oneshot::channel::<()>();
        let receiver = tokio::spawn(async move {
            let (socket, _) = listener.accept().await?;
            let mut reader = BufReader::new(socket);
            let mut line = String::new();
            reader.read_line(&mut line).await?;
            let mut length = 0;
            loop {
                line.clear();
                reader.read_line(&mut line).await?;
                if line == "\r\n" {
                    break;
                }
                if let Some((key, value)) = line.split_once(':') {
                    if key.eq_ignore_ascii_case("content-length") {
                        length = value.trim().parse::<usize>()?;
                    }
                }
            }
            let mut body = vec![0; length];
            reader.read_exact(&mut body).await?;
            reader
                .get_mut()
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 4096\r\nConnection: close\r\n\r\n")
                .await?;
            let _ = hold.await;
            Ok::<_, anyhow::Error>(())
        });
        let subscription = Subscription {
            id: "sub_receipt".into(),
            owner: unsafe { libc::geteuid() },
            url: format!("http://{address}/callback"),
            secret: format!("whsec_{}", STANDARD.encode([6; 32])),
            session_id: "project".into(),
            execution_id: None,
            expires: now() + 60,
            cursor: None,
            old_secret: None,
            pending: None,
            suspended: false,
            delivery_history: VecDeque::new(),
        };
        let result = tokio::time::timeout(
            Duration::from_millis(500),
            Sender {
                allow_loopback: true,
            }
            .post(&subscription, "evt_receipt", "{}"),
        )
        .await;
        let _ = release.send(());
        receiver.await??;
        let (status, body) =
            result.context("accepted headers must not wait for response body")??;
        assert_eq!(status, 200);
        assert!(body.is_empty());
        Ok(())
    }

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
            delivery_history: VecDeque::new(),
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
        renewed["ttlMs"] = json!(3000);
        ensure!(restored.subscribe(renewed).await?["id"] == id);
        // Verification cache is deliberately in-memory; a frontend restart verifies again.
        let (_, fresh) = tokio::time::timeout(Duration::from_secs(5), requests.recv())
            .await?
            .unwrap();
        ensure!(serde_json::from_slice::<Value>(&fresh)?["type"] == "verification");
        let child=broker.call(json!({"op":"start_execution","session_id":session,"profile":"host","io":"pipes","command":["/bin/sleep","30"]})).await?;
        let closed = broker
            .call(json!({"op":"close_session","session_id":session}))
            .await?;
        ensure!(closed["closed"] == true && closed["pending"] == false);
        let (_, terminal) = tokio::time::timeout(Duration::from_secs(2), requests.recv())
            .await?
            .unwrap();
        let terminal: Value = serde_json::from_slice(&terminal)?;
        let kinds: Vec<_> = terminal["data"]["events"]
            .as_array()
            .unwrap()
            .iter()
            .map(|event| event["kind"].as_str().unwrap())
            .collect();
        ensure!(
            kinds.contains(&"closing") && kinds.contains(&"exit") && kinds.contains(&"closed"),
            "terminal Events must remain deliverable after metadata removal"
        );
        ensure!(broker.call(json!({"op":"read_execution","execution_id":child["execution_id"],"cursor":child["cursor"]})).await?["execution"]["status"]=="exited");
        let expiry = restored.subscriptions.lock().await[&id].expires;
        ensure!(restored.subscribe(subscription.clone()).await?["id"] == id);
        ensure!(
            restored.subscriptions.lock().await[&id].expires == expiry,
            "closed leases cannot be extended indefinitely"
        );
        tokio::time::sleep(Duration::from_millis(3200)).await;
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
