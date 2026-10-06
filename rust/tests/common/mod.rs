#![allow(dead_code)]
// Local OAuth fixtures only. No real client/grant or key is created or persisted.
use anyhow::{Context, Result, ensure};
use axum::{
    Router,
    body::Bytes,
    extract::State,
    http::{Method, StatusCode, Uri, header},
    response::{IntoResponse, Response},
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use rmcp::{
    RoleClient, ServiceExt,
    model::{CallToolRequestParams, CallToolResult, ClientConfig},
    service::RunningService,
    transport::{
        StreamableHttpClientTransport, TokioChildProcess,
        streamable_http_client::StreamableHttpClientTransportConfig,
    },
};
use rsa::{RsaPrivateKey, pkcs1::EncodeRsaPrivateKey, traits::PublicKeyParts};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{
    net::TcpListener,
    process::{Child, Command},
};

pub type Client = RunningService<RoleClient, ClientConfig>;
pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}
pub fn challenge(value: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(value.as_bytes()))
}

#[derive(Clone)]
struct Code {
    challenge: String,
    resource: String,
    redirect: String,
}
#[derive(Default)]
struct Discovery {
    issuer: String,
    selected: String,
    requests: Vec<String>,
}
struct Issuer {
    base: String,
    resource: String,
    key: EncodingKey,
    jwk: Value,
    discovery: Mutex<Discovery>,
    codes: Mutex<HashMap<String, Code>>,
}
impl Issuer {
    fn token(&self, overrides: Value) -> Result<String> {
        let issuer = self.discovery.lock().unwrap().issuer.clone();
        let mut claims = json!({"iss":issuer,"aud":self.resource,"sub":"fixture-owner","scope":"mcp:tools","exp":now()+600});
        claims.as_object_mut().unwrap().extend(
            overrides
                .as_object()
                .context("claims must be an object")?
                .clone(),
        );
        let mut header = Header::new(Algorithm::RS256);
        header.kid = Some("fixture-key".into());
        header.typ = Some("at+jwt".into());
        Ok(jsonwebtoken::encode(&header, &claims, &self.key)?)
    }
}
fn json_response(status: StatusCode, value: Value) -> Response {
    (status, axum::Json(value)).into_response()
}
async fn issuer_request(
    State(state): State<Arc<Issuer>>,
    method: Method,
    uri: Uri,
    body: Bytes,
) -> Response {
    let selected = {
        let mut d = state.discovery.lock().unwrap();
        d.requests.push(uri.path().into());
        let root = d.issuer == format!("{}/", state.base);
        uri.path() == d.selected || (root && uri.path() == "/.well-known/openid-configuration")
    };
    if selected {
        let issuer = state.discovery.lock().unwrap().issuer.clone();
        return json_response(
            StatusCode::OK,
            json!({"issuer":issuer,"authorization_endpoint":format!("{}/authorize",state.base),"token_endpoint":format!("{}/token",state.base),"jwks_uri":format!("{}/jwks",state.base),"code_challenge_methods_supported":["S256"],"response_types_supported":["code"],"grant_types_supported":["authorization_code"],"token_endpoint_auth_methods_supported":["none"]}),
        );
    }
    if uri.path() == "/jwks" {
        return json_response(StatusCode::OK, json!({"keys":[state.jwk]}));
    }
    if uri.path() == "/authorize" {
        let q: HashMap<String, String> =
            url::form_urlencoded::parse(uri.query().unwrap_or("").as_bytes())
                .into_owned()
                .collect();
        let valid = q.get("response_type").is_some_and(|v| v == "code")
            && q.get("client_id").is_some_and(|v| v == "fixture-client")
            && q.get("redirect_uri")
                .is_some_and(|v| v == "http://127.0.0.1/callback")
            && q.get("resource") == Some(&state.resource)
            && q.get("code_challenge_method").is_some_and(|v| v == "S256")
            && q.contains_key("code_challenge");
        if !valid {
            return json_response(StatusCode::BAD_REQUEST, json!({"error":"invalid_request"}));
        }
        let code = uuid::Uuid::new_v4().to_string();
        state.codes.lock().unwrap().insert(
            code.clone(),
            Code {
                challenge: q["code_challenge"].clone(),
                resource: state.resource.clone(),
                redirect: q["redirect_uri"].clone(),
            },
        );
        let mut redirect = url::Url::parse(&q["redirect_uri"]).unwrap();
        redirect
            .query_pairs_mut()
            .append_pair("code", &code)
            .append_pair("state", q.get("state").map(String::as_str).unwrap_or(""))
            .append_pair("iss", &state.discovery.lock().unwrap().issuer);
        return (
            StatusCode::FOUND,
            [(header::LOCATION, redirect.to_string())],
        )
            .into_response();
    }
    if uri.path() == "/token" && method == Method::POST {
        let q: HashMap<String, String> = url::form_urlencoded::parse(&body).into_owned().collect();
        let code = state
            .codes
            .lock()
            .unwrap()
            .remove(q.get("code").map(String::as_str).unwrap_or(""));
        let valid = code.is_some_and(|c| {
            q.get("grant_type")
                .is_some_and(|v| v == "authorization_code")
                && q.get("client_id").is_some_and(|v| v == "fixture-client")
                && q.get("redirect_uri") == Some(&c.redirect)
                && q.get("resource") == Some(&c.resource)
                && challenge(q.get("code_verifier").map(String::as_str).unwrap_or(""))
                    == c.challenge
        });
        if !valid {
            return json_response(StatusCode::BAD_REQUEST, json!({"error":"invalid_grant"}));
        }
        return match state.token(json!({})) {
            Ok(token) => json_response(
                StatusCode::OK,
                json!({"access_token":token,"token_type":"Bearer","scope":"mcp:tools","expires_in":600}),
            ),
            Err(_) => json_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                json!({"error":"fixture_signing_failed"}),
            ),
        };
    }
    StatusCode::NOT_FOUND.into_response()
}

pub struct Process {
    child: Child,
    stderr: PathBuf,
}
impl Process {
    pub async fn stop(&mut self) -> Result<()> {
        if self.child.try_wait()?.is_none() {
            if let Some(pid) = self.child.id() {
                unsafe {
                    libc::kill(pid as i32, libc::SIGTERM);
                }
            }
            tokio::time::timeout(Duration::from_secs(5), self.child.wait())
                .await
                .context("server stop timed out")??;
        }
        Ok(())
    }
    pub async fn rejected(&mut self) -> Result<String> {
        let exit = tokio::time::timeout(Duration::from_secs(5), self.child.wait()).await??;
        ensure!(
            !exit.success(),
            "invalid configuration started successfully"
        );
        Ok(tokio::fs::read_to_string(&self.stderr).await?)
    }
    pub fn stderr(&self) -> Result<String> {
        Ok(std::fs::read_to_string(&self.stderr)?)
    }
}

pub struct Fixture {
    home: tempfile::TempDir,
    pub cwd: PathBuf,
    pub resource: String,
    pub http: test_http::Client,
    pub env: HashMap<String, String>,
    issuer: Arc<Issuer>,
    issuer_task: tokio::task::JoinHandle<()>,
    binary: PathBuf,
}
impl Fixture {
    pub async fn new() -> Result<Self> {
        let home = tempfile::Builder::new()
            .prefix("dsm-rust-")
            .tempdir_in("/tmp")?;
        let cwd = home.path().join("project");
        tokio::fs::create_dir(&cwd).await?;
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let base = format!("http://{}", listener.local_addr()?);
        let probe = TcpListener::bind("127.0.0.1:0").await?;
        let resource = format!("http://{}/mcp", probe.local_addr()?);
        drop(probe);
        let private = RsaPrivateKey::new(&mut rand::thread_rng(), 2048)?;
        let jwk = json!({"kty":"RSA","n":URL_SAFE_NO_PAD.encode(private.n().to_bytes_be()),"e":URL_SAFE_NO_PAD.encode(private.e().to_bytes_be()),"kid":"fixture-key","alg":"RS256","use":"sig"});
        let key = EncodingKey::from_rsa_der(private.to_pkcs1_der()?.as_bytes());
        let issuer = Arc::new(Issuer {
            base: base.clone(),
            resource: resource.clone(),
            key,
            jwk,
            discovery: Mutex::new(Discovery {
                issuer: format!("{base}/"),
                selected: "/.well-known/oauth-authorization-server".into(),
                requests: vec![],
            }),
            codes: Mutex::new(HashMap::new()),
        });
        let app = Router::new()
            .fallback(issuer_request)
            .with_state(issuer.clone());
        let issuer_task = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        let env = HashMap::from([
            (
                "PATH".into(),
                std::env::var("PATH").unwrap_or("/usr/bin:/bin".into()),
            ),
            ("HOME".into(), home.path().display().to_string()),
            ("LANG".into(), "C.UTF-8".into()),
            (
                "XDG_STATE_HOME".into(),
                home.path().join("xdg").display().to_string(),
            ),
            (
                "DEV_SESSION_MCP_STATE_DIR".into(),
                home.path().join("state").display().to_string(),
            ),
            (
                "DEV_SESSION_MCP_TMUX_BIN".into(),
                std::env::var("DEV_SESSION_MCP_TMUX_BIN").unwrap_or("tmux".into()),
            ),
        ]);
        let binary = std::env::var_os("DEV_SESSION_MCP_RUST_BIN")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_dev-session-mcp")));
        Ok(Self {
            home,
            cwd,
            resource,
            http: test_http::Client::builder()
                .timeout(Duration::from_secs(35))
                .redirect(test_http::redirect::Policy::none())
                .build()?,
            env,
            issuer,
            issuer_task,
            binary,
        })
    }
    pub fn issuer(&self) -> String {
        self.issuer.discovery.lock().unwrap().issuer.clone()
    }
    pub fn token(&self, overrides: Value) -> Result<String> {
        self.issuer.token(overrides)
    }
    pub fn discovery(&self, variant: &str) -> Vec<String> {
        let suffix = format!(
            "/tenant/{variant}{}",
            if variant == "oidc-trailing" { "/" } else { "" }
        );
        let expected = vec![
            format!("/.well-known/oauth-authorization-server{suffix}"),
            format!("/.well-known/openid-configuration{suffix}"),
            format!(
                "{}/.well-known/openid-configuration",
                suffix.trim_end_matches('/')
            ),
        ];
        let mut d = self.issuer.discovery.lock().unwrap();
        d.issuer = format!("{}{suffix}", self.issuer.base);
        d.requests.clear();
        d.selected = expected[if variant == "oauth" {
            0
        } else if variant == "oidc-appended" {
            2
        } else {
            1
        }]
        .clone();
        expected
    }
    pub fn requests(&self) -> Vec<String> {
        self.issuer.discovery.lock().unwrap().requests.clone()
    }
    pub async fn config(&self, subjects: Value) -> Result<PathBuf> {
        let path = self.home.path().join("http.json");
        tokio::fs::write(&path,serde_json::to_vec(&json!({"listen":self.resource.strip_prefix("http://").unwrap().strip_suffix("/mcp").unwrap(),"resource":self.resource,"issuer":self.issuer(),"allowed_subjects":subjects,"local_fixture":true}))?).await?;
        Ok(path)
    }
    pub async fn start(&self, subjects: Value) -> Result<Process> {
        let config = self.config(subjects).await?;
        let stderr = self
            .home
            .path()
            .join(format!("stderr-{}.log", uuid::Uuid::new_v4()));
        let file = std::fs::File::create(&stderr)?;
        let child = Command::new(&self.binary)
            .args(["serve", "--config"])
            .arg(config)
            .env_clear()
            .envs(&self.env)
            .kill_on_drop(true)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(file)
            .spawn()?;
        Ok(Process { child, stderr })
    }
    pub async fn ready(&self, process: &mut Process) -> Result<()> {
        for _ in 0..100 {
            ensure!(
                process.child.try_wait()?.is_none(),
                "Rust server failed: {}",
                process.stderr()?
            );
            if let Ok(response) = self.http.get(&self.resource).send().await {
                if response.status().as_u16() == 401 {
                    return Ok(());
                }
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        anyhow::bail!("HTTP startup timed out");
    }
    /// Hand-written fixture exchange, not SDK automatic OAuth linking.
    pub async fn authorize(
        &self,
        wrong_verifier: bool,
        wrong_resource: bool,
    ) -> Result<(u16, Value)> {
        let verifier = uuid::Uuid::new_v4().to_string() + &uuid::Uuid::new_v4().to_string();
        let state = uuid::Uuid::new_v4().to_string();
        let response = self
            .http
            .get(format!("{}/authorize", self.issuer.base))
            .query(&[
                ("response_type", "code"),
                ("client_id", "fixture-client"),
                ("redirect_uri", "http://127.0.0.1/callback"),
                ("scope", "mcp:tools"),
                ("resource", &self.resource),
                ("state", &state),
                ("code_challenge", &challenge(&verifier)),
                ("code_challenge_method", "S256"),
            ])
            .send()
            .await?;
        ensure!(
            response.status().as_u16() == 302,
            "fixture authorize failed"
        );
        let redirect = url::Url::parse(
            response
                .headers()
                .get("location")
                .context("missing redirect")?
                .to_str()?,
        )?;
        let q: HashMap<String, String> = redirect.query_pairs().into_owned().collect();
        ensure!(
            q["state"] == state && q["iss"] == self.issuer(),
            "fixture callback mismatch"
        );
        let form: Vec<(&str, &str)> = vec![
            ("grant_type", "authorization_code"),
            ("client_id", "fixture-client"),
            ("redirect_uri", "http://127.0.0.1/callback"),
            ("code", &q["code"]),
            (
                "code_verifier",
                if wrong_verifier {
                    "incorrect"
                } else {
                    &verifier
                },
            ),
            (
                "resource",
                if wrong_resource {
                    "https://wrong.invalid/mcp"
                } else {
                    &self.resource
                },
            ),
        ];
        // reqwest's form feature is unnecessary: use the standard URL encoder.
        let mut encoded = url::form_urlencoded::Serializer::new(String::new());
        for (k, v) in form {
            encoded.append_pair(k, v);
        }
        let response = self
            .http
            .post(format!("{}/token", self.issuer.base))
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(encoded.finish())
            .send()
            .await?;
        Ok((response.status().as_u16(), response.json().await?))
    }
    pub async fn connect(&self, access: &str) -> Result<Client> {
        let config = StreamableHttpClientTransportConfig::with_uri(self.resource.clone())
            .auth_header(access);
        Ok(ClientConfig::default()
            .serve(StreamableHttpClientTransport::with_client(
                self.http.clone(),
                config,
            ))
            .await?)
    }
    pub async fn stdio(&self) -> Result<Client> {
        let mut command = Command::new(&self.binary);
        command.arg("stdio").env_clear().envs(&self.env);
        Ok(ClientConfig::default()
            .serve(TokioChildProcess::new(command)?)
            .await?)
    }
    pub async fn leaked_environment_rejected(&self) -> Result<()> {
        let status = Command::new(&self.binary)
            .arg("stdio")
            .env_clear()
            .envs(&self.env)
            .env("CONTROL_PLANE_API_KEY", "fixture-not-a-key")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await?;
        ensure!(!status.success(), "transport environment was accepted");
        Ok(())
    }
    pub async fn modern(&self, access: &str, method: &str, mut params: Value) -> Result<Value> {
        params["_meta"] = json!({"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientInfo":{"name":"rust-fixture","version":"1"},"io.modelcontextprotocol/clientCapabilities":{}});
        let mut request = self
            .http
            .post(&self.resource)
            .bearer_auth(access)
            .header("Accept", "application/json, text/event-stream")
            .header("MCP-Protocol-Version", "2026-07-28")
            .header("Mcp-Method", method);
        if let Some(name) = params["name"].as_str() {
            request = request.header("Mcp-Name", name);
        }
        let response = request
            .json(&json!({"jsonrpc":"2.0","id":1,"method":method,"params":params}))
            .send()
            .await?;
        ensure!(response.status().as_u16() == 200, "modern HTTP failed");
        ensure!(
            !response.headers().contains_key("mcp-session-id"),
            "modern request created session header"
        );
        let result: Value = response.json().await?;
        ensure!(
            result.get("error").is_none(),
            "modern request returned protocol error"
        );
        Ok(result["result"].clone())
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.issuer_task.abort();
        let _ = std::process::Command::new(&self.env["DEV_SESSION_MCP_TMUX_BIN"])
            .arg("-S")
            .arg(Path::new(&self.env["DEV_SESSION_MCP_STATE_DIR"]).join("tmux.sock"))
            .arg("kill-server")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}
pub async fn raw(client: &Client, name: &str, args: Value) -> Result<CallToolResult> {
    Ok(client
        .call_tool(
            CallToolRequestParams::new(name.to_owned()).with_arguments(
                args.as_object()
                    .context("tool args must be an object")?
                    .clone(),
            ),
        )
        .await?)
}
pub fn result_text(result: &CallToolResult) -> Result<String> {
    Ok(serde_json::to_value(result)?["content"][0]["text"]
        .as_str()
        .context("missing tool text")?
        .into())
}
pub async fn call(client: &Client, name: &str, args: Value) -> Result<Value> {
    let result = raw(client, name, args).await?;
    ensure!(
        result.is_error != Some(true),
        "tool failed: {}",
        result_text(&result)?
    );
    Ok(serde_json::from_str(&result_text(&result)?)?)
}
pub async fn until(
    client: &Client,
    args: Value,
    predicate: impl Fn(&Value) -> bool,
) -> Result<Value> {
    for _ in 0..100 {
        let result = call(client, "read_output", args.clone()).await?;
        if predicate(&result) {
            return Ok(result);
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    anyhow::bail!("terminal wait expired");
}
