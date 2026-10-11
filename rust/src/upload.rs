//! One-time upload URLs for clients that cannot pass attachments to
//! `import_file` (claude.ai): `create_upload` issues a short-lived URL bound to
//! one destination, and the client's own code sandbox PUTs the bytes to it, so
//! the file never passes through model context. Only the HTTP frontend serves
//! the URL, and only when an operator sets `--upload-base-url`.
use crate::{
    config,
    files::{self, MAX_BYTES, Sink},
    workspace::text,
};
use anyhow::{Context, Result, ensure};
use axum::{
    body::Body,
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
};
use futures_util::StreamExt;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use url::Url;

const TTL: Duration = Duration::from_secs(15 * 60);
const DEADLINE: Duration = Duration::from_secs(600);
const MAX_PENDING: usize = 64;

pub fn tool() -> Value {
    json!({
        "name":"create_upload",
        "description":"Issue a single-use HTTPS upload URL (valid 15 minutes, up to 128 MiB) that stores one file at path inside the session project, for clients whose attachments cannot reach import_file (e.g. claude.ai). Send the bytes from the client's own code sandbox with `curl -fsS -T FILE URL` (HTTP PUT) instead of passing them through the conversation; that sandbox must be allowed to reach this host. The destination parent must exist; existing files are kept unless overwrite=true. The PUT response and later list_directory show path, size and SHA256.",
        "inputSchema":{"type":"object","properties":{
            "session_id":{"type":"string","maxLength":64},
            "path":{"type":"string","maxLength":4096},
            "overwrite":{"type":"boolean","default":false},
            "expected_sha256":{"type":"string","pattern":"^[A-Fa-f0-9]{64}$"}
        },"required":["session_id","path"],"additionalProperties":false},
        "annotations":{"readOnlyHint":false,"destructiveHint":true,"openWorldHint":true}
    })
}

struct Pending {
    session_id: String,
    destination: PathBuf,
    overwrite: bool,
    expected: Option<String>,
    expires: Instant,
}

pub struct Uploads {
    base: Url,
    /// Keyed by SHA256 of the token so the map never holds usable secrets.
    pending: Mutex<HashMap<String, Pending>>,
}

fn key(token: &str) -> String {
    format!("{:x}", Sha256::digest(token.as_bytes()))
}

impl Uploads {
    pub fn new(base: &str) -> Result<Arc<Self>> {
        let mut base = Url::parse(base).context("invalid --upload-base-url")?;
        let loopback = base.host_str().is_some_and(|h| {
            h == "localhost"
                || h.parse::<std::net::IpAddr>()
                    .is_ok_and(|ip| ip.is_loopback())
        });
        ensure!(
            (base.scheme() == "https" || (base.scheme() == "http" && loopback))
                && base.username().is_empty()
                && base.password().is_none()
                && base.query().is_none()
                && base.fragment().is_none(),
            "--upload-base-url must be an https URL without userinfo, query or fragment"
        );
        if !base.path().ends_with('/') {
            base.set_path(&format!("{}/", base.path()));
        }
        Ok(Arc::new(Self {
            base,
            pending: Mutex::new(HashMap::new()),
        }))
    }

    pub async fn create(&self, args: &Value) -> Result<Value> {
        let session_id = text(args, "session_id")?;
        let session = config::load_session(session_id).await?;
        ensure!(
            session.state == config::SessionState::Open,
            "session is closing"
        );
        let (overwrite, expected) = files::write_options(args)?;
        let destination = files::destination(&session.cwd, text(args, "path")?, overwrite)?;
        let token = format!(
            "{}{}",
            uuid::Uuid::new_v4().simple(),
            uuid::Uuid::new_v4().simple()
        );
        let url = self.base.join(&format!("upload/{token}"))?;
        {
            let mut pending = self.pending.lock().unwrap();
            let now = Instant::now();
            pending.retain(|_, p| p.expires > now);
            ensure!(
                pending.len() < MAX_PENDING,
                "too many pending uploads; use or let some expire first"
            );
            pending.insert(
                key(&token),
                Pending {
                    session_id: session_id.to_owned(),
                    destination: destination.clone(),
                    overwrite,
                    expected: expected.map(str::to_owned),
                    expires: now + TTL,
                },
            );
        }
        Ok(json!({
            "upload_url": url.as_str(),
            "method": "PUT",
            "path": destination,
            "expires_in_seconds": TTL.as_secs(),
            "max_bytes": MAX_BYTES,
            "single_use": true,
            "example": format!("curl -fsS -T ./FILE '{url}'"),
        }))
    }

    /// Handles `PUT /upload/{token}`. The token is consumed before the body is
    /// read, so a failed transfer needs a new `create_upload`.
    pub async fn receive(&self, token: &str, headers: &HeaderMap, body: Body) -> Response {
        let pending = {
            let mut pending = self.pending.lock().unwrap();
            pending
                .remove(&key(token))
                .filter(|p| p.expires > Instant::now())
        };
        let Some(pending) = pending else {
            return (StatusCode::NOT_FOUND, "unknown or expired upload URL\n").into_response();
        };
        match tokio::time::timeout(DEADLINE, store(pending, headers, body)).await {
            Ok(Ok(result)) => (
                StatusCode::CREATED,
                [(header::CONTENT_TYPE, "application/json")],
                format!("{result}\n"),
            )
                .into_response(),
            Ok(Err(error)) => {
                (StatusCode::BAD_REQUEST, format!("upload failed: {error}\n")).into_response()
            }
            Err(_) => (StatusCode::REQUEST_TIMEOUT, "upload timed out\n").into_response(),
        }
    }
}

async fn store(pending: Pending, headers: &HeaderMap, body: Body) -> Result<Value> {
    let session = config::load_session(&pending.session_id).await?;
    ensure!(
        session.state == config::SessionState::Open,
        "session is closing"
    );
    ensure!(
        headers
            .get(header::CONTENT_ENCODING)
            .is_none_or(|v| v == "identity"),
        "compressed upload is unsupported"
    );
    let length = headers
        .get(header::CONTENT_LENGTH)
        .map(|v| v.to_str().ok().and_then(|v| v.parse::<u64>().ok()))
        .map(|v| v.context("invalid Content-Length"))
        .transpose()?;
    ensure!(
        length.is_none_or(|n| n <= MAX_BYTES),
        "attachment exceeds byte limit"
    );
    let mut sink = Sink::create(&pending.destination, MAX_BYTES).await?;
    let mut stream = body.into_data_stream();
    while let Some(chunk) = stream.next().await {
        sink.write(&chunk.map_err(|_| anyhow::anyhow!("upload body transfer failed"))?)
            .await?;
    }
    sink.finish(
        pending.destination,
        pending.overwrite,
        pending.expected.as_deref(),
        length,
    )
    .await
}
