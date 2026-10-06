use anyhow::{Context, Result, ensure};
use axum::{
    extract::{Request, State},
    http::{StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
};
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header, jwk::JwkSet};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    net::SocketAddr,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::Mutex;
use url::Url;

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    pub listen: SocketAddr,
    /// Canonical resource identifier AND expected JWT audience, including /mcp.
    pub resource: String,
    /// Exact trusted issuer from the AS discovery document (trailing slash matters).
    pub issuer: String,
    /// Preserve the configuration name; exactly one owner is supported.
    pub allowed_subjects: Vec<String>,
    #[serde(default = "default_scope")]
    pub scope: String,
    #[serde(default)]
    pub allowed_origins: Vec<String>,
    /// Only permits HTTP URLs whose host is a loopback IP; authentication stays on.
    #[serde(default)]
    pub local_fixture: bool,
}
fn default_scope() -> String {
    "mcp:tools".into()
}

fn checked_url(value: &str, fixture: bool) -> Result<Url> {
    let u = Url::parse(value)?;
    ensure!(
        u.username().is_empty()
            && u.password().is_none()
            && u.query().is_none()
            && u.fragment().is_none(),
        "URL must not contain credentials, query, or fragment"
    );
    let loopback = u
        .host_str()
        .and_then(|s| s.parse::<std::net::IpAddr>().ok())
        .is_some_and(|v| v.is_loopback());
    ensure!(
        u.scheme() == "https" || (fixture && u.scheme() == "http" && loopback),
        "HTTPS URL required; HTTP fixture must use a loopback IP"
    );
    Ok(u)
}

struct KeyCache {
    keys: JwkSet,
    fetched: Instant,
}
pub struct Auth {
    pub settings: Settings,
    pub metadata: Value,
    challenge: String,
    http: reqwest::Client,
    jwks_uri: Url,
    keys: Mutex<KeyCache>,
}

async fn document(http: &reqwest::Client, url: Url) -> Result<Value> {
    let mut response = http.get(url).send().await?.error_for_status()?;
    let mut data = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        ensure!(
            data.len() + chunk.len() <= 1024 * 1024,
            "discovery/JWKS document exceeds 1 MiB"
        );
        data.extend_from_slice(&chunk);
    }
    Ok(serde_json::from_slice(&data)?)
}

impl Auth {
    pub async fn load(settings: Settings) -> Result<Arc<Self>> {
        ensure!(
            settings.listen.ip().is_loopback(),
            "HTTP backend must listen on a loopback IP; use an HTTPS reverse proxy"
        );
        ensure!(
            settings.allowed_subjects.len() == 1 && !settings.allowed_subjects[0].is_empty(),
            "allowed_subjects must contain exactly one non-empty subject; this server is single-owner"
        );
        ensure!(
            !settings.scope.is_empty()
                && settings
                    .scope
                    .bytes()
                    .all(|b| b.is_ascii_graphic() && b != b'"' && b != b'\\'),
            "invalid scope"
        );
        let resource = checked_url(&settings.resource, settings.local_fixture)?;
        ensure!(resource.path() == "/mcp", "resource URL must end at /mcp");
        let issuer = checked_url(&settings.issuer, settings.local_fixture)?;
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        // MCP discovery order: OAuth insertion, OIDC insertion, OIDC appending.
        // Never discover from an unverified token claim.
        let base = issuer.origin().ascii_serialization();
        let issuer_path = if issuer.path() == "/" {
            ""
        } else {
            issuer.path()
        };
        let oauth_url = format!(
            "{base}/.well-known/oauth-authorization-server{}",
            issuer_path
        );
        let oidc_inserted = format!("{base}/.well-known/openid-configuration{issuer_path}");
        let oidc_appended = format!(
            "{}/.well-known/openid-configuration",
            settings.issuer.trim_end_matches('/')
        );
        let mut candidates = vec![oauth_url, oidc_inserted];
        if !candidates.contains(&oidc_appended) {
            candidates.push(oidc_appended);
        }
        let mut found = None;
        for candidate in candidates {
            if let Ok(doc) = document(&http, checked_url(&candidate, settings.local_fixture)?).await
            {
                found = Some(doc);
                break;
            }
        }
        let doc = found.context("authorization server discovery failed")?;
        ensure!(
            doc["issuer"].as_str() == Some(&settings.issuer),
            "discovery issuer does not exactly match configured issuer"
        );
        ensure!(
            doc["code_challenge_methods_supported"]
                .as_array()
                .is_some_and(|a| a.iter().any(|v| v == "S256")),
            "authorization server must advertise PKCE S256"
        );
        for field in ["authorization_endpoint", "token_endpoint"] {
            checked_url(
                doc[field]
                    .as_str()
                    .context("missing authorization/token endpoint")?,
                settings.local_fixture,
            )?;
        }
        let jwks_uri = checked_url(
            doc["jwks_uri"].as_str().context("missing jwks_uri")?,
            settings.local_fixture,
        )?;
        let keys = serde_json::from_value(document(&http, jwks_uri.clone()).await?)?;
        let metadata_url = format!(
            "{}/.well-known/oauth-protected-resource/mcp",
            resource.origin().ascii_serialization()
        );
        let challenge = format!(
            "Bearer resource_metadata=\"{metadata_url}\", scope=\"{}\"",
            settings.scope
        );
        let metadata = json!({"resource":settings.resource,"authorization_servers":[settings.issuer],"scopes_supported":[settings.scope],"bearer_methods_supported":["header"]});
        Ok(Arc::new(Self {
            settings,
            metadata,
            challenge,
            http,
            jwks_uri,
            keys: Mutex::new(KeyCache {
                keys,
                fetched: Instant::now(),
            }),
        }))
    }

    async fn verify(&self, token: &str) -> std::result::Result<(), StatusCode> {
        if token.len() > 16384 {
            return Err(StatusCode::UNAUTHORIZED);
        }
        let header = decode_header(token).map_err(|_| StatusCode::UNAUTHORIZED)?;
        // Deliberately one asymmetric algorithm. Never accept HS*, jku, or x5u.
        if header.alg != Algorithm::RS256 {
            return Err(StatusCode::UNAUTHORIZED);
        }
        let kid = header.kid.ok_or(StatusCode::UNAUTHORIZED)?;
        let key = {
            let mut cache = self.keys.lock().await;
            let missing = cache.keys.find(&kid).is_none();
            if cache.fetched.elapsed() >= Duration::from_secs(300)
                || (missing && cache.fetched.elapsed() >= Duration::from_secs(30))
            {
                let value = document(&self.http, self.jwks_uri.clone())
                    .await
                    .map_err(|_| StatusCode::UNAUTHORIZED)?;
                cache.keys = serde_json::from_value(value).map_err(|_| StatusCode::UNAUTHORIZED)?;
                cache.fetched = Instant::now();
            }
            let jwk = cache.keys.find(&kid).ok_or(StatusCode::UNAUTHORIZED)?;
            let info = serde_json::to_value(jwk).map_err(|_| StatusCode::UNAUTHORIZED)?;
            if info["kty"] != "RSA"
                || info.get("use").is_some_and(|v| v != "sig")
                || info.get("alg").is_some_and(|v| v != "RS256")
                || info.get("key_ops").is_some_and(|v| {
                    !v.as_array()
                        .is_some_and(|a| a.iter().any(|v| v == "verify"))
                })
            {
                return Err(StatusCode::UNAUTHORIZED);
            }
            DecodingKey::from_jwk(jwk).map_err(|_| StatusCode::UNAUTHORIZED)?
        };
        let mut validation = Validation::new(Algorithm::RS256);
        validation.set_issuer(&[&self.settings.issuer]);
        validation.set_audience(&[&self.settings.resource]);
        validation.set_required_spec_claims(&["exp", "iss", "aud", "sub"]);
        validation.validate_nbf = true;
        validation.leeway = 30;
        let claims = decode::<Value>(token, &key, &validation)
            .map_err(|_| StatusCode::UNAUTHORIZED)?
            .claims;
        if !claims["sub"]
            .as_str()
            .is_some_and(|sub| sub == self.settings.allowed_subjects[0])
        {
            return Err(StatusCode::FORBIDDEN);
        }
        let scope_ok = claims["scope"]
            .as_str()
            .is_some_and(|s| s.split_whitespace().any(|s| s == self.settings.scope))
            || claims["scp"]
                .as_array()
                .is_some_and(|a| a.iter().any(|v| v.as_str() == Some(&self.settings.scope)));
        if !scope_ok {
            return Err(StatusCode::FORBIDDEN);
        }
        Ok(())
    }
}

pub async fn authenticate(State(auth): State<Arc<Auth>>, request: Request, next: Next) -> Response {
    // Never accept bearer tokens via URL, cookie, or a forwarded proxy identity.
    let result = if request.uri().query().is_some() {
        Err(StatusCode::UNAUTHORIZED)
    } else {
        let headers: Vec<_> = request
            .headers()
            .get_all(header::AUTHORIZATION)
            .iter()
            .collect();
        if headers.len() != 1 {
            Err(StatusCode::UNAUTHORIZED)
        } else {
            match headers[0]
                .to_str()
                .ok()
                .and_then(|s| s.split_once(' '))
                .filter(|(scheme, token)| {
                    scheme.eq_ignore_ascii_case("Bearer")
                        && !token.is_empty()
                        && !token.bytes().any(|b| b.is_ascii_whitespace())
                }) {
                Some((_, token)) => auth.verify(token).await,
                None => Err(StatusCode::UNAUTHORIZED),
            }
        }
    };
    match result {
        Ok(()) => next.run(request).await,
        Err(status) => {
            let mut response = (status, "OAuth authorization required").into_response();
            let value = if status == StatusCode::FORBIDDEN {
                format!("{}, error=\"insufficient_scope\"", auth.challenge)
            } else {
                auth.challenge.clone()
            };
            response.headers_mut().insert(
                header::WWW_AUTHENTICATE,
                value.parse().expect("validated challenge"),
            );
            response
                .headers_mut()
                .insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
            response
        }
    }
}
