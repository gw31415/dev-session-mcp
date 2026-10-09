//! Streamable HTTP frontend. It has no authentication of its own: bind it to
//! loopback and publish it only through an authenticating proxy such as
//! Cloudflare Tunnel + Access.
use crate::server::{Server, advertise_events};
use anyhow::{Result, ensure};
use axum::{
    body::{Body, Bytes, to_bytes},
    extract::Request,
    http::header,
    middleware::{self, Next},
    response::Response,
    routing::get,
};
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};
use serde_json::Value;
use std::{net::SocketAddr, sync::Arc};

const MAX_BODY: usize = 4 * 1024 * 1024;

pub struct Options {
    pub listen: SocketAddr,
    pub allowed_hosts: Vec<String>,
    pub allowed_origins: Vec<String>,
    pub allow_remote_bind: bool,
}

pub async fn serve(options: Options) -> Result<()> {
    ensure!(
        options.listen.ip().is_loopback() || options.allow_remote_bind,
        "refusing to bind {} without authentication; put an authenticating proxy in front and pass --allow-remote-bind",
        options.listen
    );
    let server = Server::new().await?;
    let mut hosts = vec!["localhost".to_owned(), "127.0.0.1".into(), "::1".into()];
    hosts.extend(options.allowed_hosts);
    // Stateless JSON responses serve every protocol revision on any number of
    // concurrent clients; the broker holds all durable state.
    let config = StreamableHttpServerConfig::default()
        .with_legacy_session_mode(false)
        .with_json_response(true)
        .with_allowed_hosts(hosts)
        .with_allowed_origins(options.allowed_origins)
        .enforce_origin_validation()
        .with_max_request_body_bytes(MAX_BODY);
    let mcp = StreamableHttpService::new(
        move || Ok(server.clone()),
        Arc::new(LocalSessionManager::default()),
        config,
    );
    let app = axum::Router::new()
        .route_service("/mcp", mcp)
        .route("/healthz", get(|| async { "ok" }))
        .layer(middleware::from_fn(discovery));
    let listener = tokio::net::TcpListener::bind(options.listen).await?;
    eprintln!("dev-session-mcp listening on http://{}/mcp", options.listen);
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}

/// Adds the Events capability to `server/discover` responses (JSON or SSE).
async fn discovery(request: Request, next: Next) -> Response {
    if request.method() != axum::http::Method::POST {
        return next.run(request).await;
    }
    let (parts, body) = request.into_parts();
    let Ok(bytes) = to_bytes(body, MAX_BODY).await else {
        return Response::builder()
            .status(413)
            .body(Body::from("request body too large"))
            .unwrap();
    };
    let is_discover =
        serde_json::from_slice::<Value>(&bytes).is_ok_and(|v| v["method"] == "server/discover");
    let response = next
        .run(Request::from_parts(parts, Body::from(bytes)))
        .await;
    if !is_discover {
        return response;
    }
    let (mut parts, body) = response.into_parts();
    let Ok(bytes) = to_bytes(body, MAX_BODY).await else {
        return Response::builder().status(502).body(Body::empty()).unwrap();
    };
    let patched = patch(&bytes).unwrap_or(bytes);
    parts.headers.remove(header::CONTENT_LENGTH);
    Response::from_parts(parts, Body::from(patched))
}

fn patch(bytes: &Bytes) -> Option<Bytes> {
    let text = std::str::from_utf8(bytes).ok()?;
    let patch_json = |json: &str| -> Option<String> {
        let mut value: Value = serde_json::from_str(json).ok()?;
        advertise_events(value.get_mut("result")?);
        serde_json::to_string(&value).ok()
    };
    if let Some(json) = patch_json(text) {
        return Some(json.into());
    }
    // Server-sent events: rewrite each `data:` line holding the result.
    let lines: Vec<String> = text
        .split('\n')
        .map(|line| {
            line.strip_prefix("data:")
                .and_then(|data| patch_json(data.trim_start()))
                .map(|json| format!("data: {json}"))
                .unwrap_or_else(|| line.to_owned())
        })
        .collect();
    Some(lines.join("\n").into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovery_patch_handles_json_and_sse() {
        let json = Bytes::from(r#"{"jsonrpc":"2.0","id":1,"result":{"capabilities":{}}}"#);
        let value: Value = serde_json::from_slice(&patch(&json).unwrap()).unwrap();
        assert_eq!(
            value["result"]["capabilities"]["events"],
            serde_json::json!({})
        );
        let sse = Bytes::from(
            "id: 0\nretry: 3000\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"capabilities\":{}}}\n\n",
        );
        let patched = String::from_utf8(patch(&sse).unwrap().to_vec()).unwrap();
        assert!(patched.contains(r#""events":{}"#) && patched.starts_with("id: 0\nretry: 3000\n"));
    }
}
