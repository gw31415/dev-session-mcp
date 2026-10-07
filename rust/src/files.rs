use crate::{config, workspace::text};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{net::IpAddr, path::PathBuf, time::Duration};
use tokio::io::AsyncWriteExt;
use url::Url;

const MAX_BYTES: u64 = 128 * 1024 * 1024;
const DEADLINE: Duration = Duration::from_secs(120);

pub fn tool() -> Value {
    json!({
        "name":"import_file",
        "description":"Save a ChatGPT-provided attachment directly into the session project without putting file bytes in model context. Requires administrator-configured trusted HTTPS attachment origins. No extraction or execution. Destination must have an existing parent inside the session cwd; existing files are preserved unless overwrite=true. Returns path, size and SHA256. No partial resume; failed transfers are discarded.",
        "inputSchema":{"type":"object","properties":{
            "session_id":{"type":"string","maxLength":64},
            "file":{"type":"object","properties":{
                "download_url":{"type":"string","maxLength":16384},
                "file_id":{"type":"string","maxLength":1024},
                "mime_type":{"type":"string"},"file_name":{"type":"string"}
            },"required":["download_url","file_id"],"additionalProperties":false},
            "path":{"type":"string","maxLength":4096},
            "overwrite":{"type":"boolean","default":false},
            "expected_sha256":{"type":"string","pattern":"^[A-Fa-f0-9]{64}$"}
        },"required":["session_id","file","path"],"additionalProperties":false},
        "annotations":{"readOnlyHint":false,"destructiveHint":true,"openWorldHint":true},
        "_meta":{"openai/fileParams":["file"]}
    })
}

fn https_url(value: &str) -> Result<Url> {
    ensure!(value.len() <= 16384, "attachment URL too long");
    let url = Url::parse(value).map_err(|_| anyhow::anyhow!("invalid attachment URL"))?;
    ensure!(
        url.scheme() == "https"
            && url.port_or_known_default() == Some(443)
            && url.username().is_empty()
            && url.password().is_none()
            && url.fragment().is_none()
            && matches!(url.host(), Some(url::Host::Domain(_))),
        "attachment URL must use HTTPS on port 443 with a DNS hostname and no userinfo or fragment"
    );
    Ok(url)
}
fn authorized_url(value: &str, origins: &str) -> Result<Url> {
    let url = https_url(value)?;
    let mut allowed = false;
    for origin in origins.split(',').filter(|s| !s.trim().is_empty()) {
        let trusted = https_url(origin.trim())?;
        ensure!(
            trusted.path() == "/" && trusted.query().is_none(),
            "configured attachment origins must have no path or query"
        );
        allowed |= trusted.origin() == url.origin();
    }
    ensure!(
        allowed,
        "attachment origin not configured or permitted; administrator must set DEV_SESSION_MCP_FILE_ORIGINS"
    );
    Ok(url)
}
pub(crate) fn public_address(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            let [a, b, c, _] = ip.octets();
            !(a == 0
                || a == 10
                || a == 127
                || a >= 224
                || (a == 100 && (64..=127).contains(&b))
                || (a == 169 && b == 254)
                || (a == 172 && (16..=31).contains(&b))
                || (a == 192
                    && ((b == 0 && (c == 0 || c == 2)) || (b == 88 && c == 99) || b == 168))
                || (a == 198 && ((18..=19).contains(&b) || (b == 51 && c == 100)))
                || (a == 203 && b == 0 && c == 113))
        }
        IpAddr::V6(ip) => {
            let s = ip.segments();
            // Conservative public unicast policy excludes tunnelling and special-purpose ranges.
            (0x2000..=0x3fff).contains(&s[0])
                && s[0] != 0x2002
                && !(s[0] == 0x2001 && (s[1] <= 0x01ff || s[1] == 0x0db8))
                && !(s[0] == 0x3fff && s[1] <= 0x0fff)
        }
    }
}
struct Partial(PathBuf);
impl Drop for Partial {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

pub async fn import(args: &Value) -> Result<Value> {
    let session = config::load_session(text(args, "session_id")?).await?;
    let origins = std::env::var("DEV_SESSION_MCP_FILE_ORIGINS").unwrap_or_default();
    let url = authorized_url(text(&args["file"], "download_url")?, &origins)?;
    ensure!(
        !text(&args["file"], "file_id")?.is_empty(),
        "file_id is required"
    );
    let expected = args
        .get("expected_sha256")
        .map(|v| v.as_str().context("invalid expected_sha256"))
        .transpose()?;
    if let Some(hash) = expected {
        ensure!(
            hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()),
            "expected_sha256 must be 64 hex digits"
        );
    }
    let overwrite = args
        .get("overwrite")
        .map(|v| v.as_bool().context("invalid overwrite"))
        .transpose()?
        .unwrap_or(false);
    let path = session.cwd.join(text(args, "path")?);
    let parent = std::fs::canonicalize(path.parent().context("destination needs a parent")?)?;
    ensure!(
        parent.starts_with(&session.cwd),
        "destination parent must be inside the session project"
    );
    let name = path.file_name().context("destination needs a filename")?;
    ensure!(name != "." && name != "..", "invalid destination filename");
    let destination = parent.join(name);
    let existing = match std::fs::symlink_metadata(&destination) {
        Ok(metadata) => Some(metadata),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    ensure!(
        existing.is_none() || overwrite,
        "destination exists; overwrite must be explicit"
    );
    ensure!(
        existing.as_ref().is_none_or(|m| m.is_file()),
        "destination must be a regular file"
    );
    tokio::time::timeout(DEADLINE, download(url, destination, overwrite, expected))
        .await
        .context("attachment transfer timed out")?
}
async fn download(
    url: Url,
    destination: PathBuf,
    overwrite: bool,
    expected: Option<&str>,
) -> Result<Value> {
    let host = url.host_str().context("missing attachment hostname")?;
    let addresses: Vec<_> = tokio::net::lookup_host((host, 443))
        .await
        .map_err(|_| anyhow::anyhow!("attachment hostname resolution failed"))?
        .collect();
    ensure!(
        !addresses.is_empty() && addresses.iter().all(|a| public_address(a.ip())),
        "attachment hostname resolves to a prohibited address"
    );
    // Pin validated addresses so a second DNS lookup cannot rebind to a private host.
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .resolve_to_addrs(host, &addresses)
        .connect_timeout(Duration::from_secs(10))
        .read_timeout(Duration::from_secs(20))
        .timeout(DEADLINE)
        .no_gzip()
        .no_brotli()
        .no_deflate()
        .no_zstd()
        .build()?;
    // URL signatures are capabilities; never retain reqwest's URL-bearing errors.
    let response = client
        .get(url)
        .header("Accept-Encoding", "identity")
        .send()
        .await
        .map_err(|_| anyhow::anyhow!("attachment HTTPS request failed"))?;
    save_response(response, destination, overwrite, expected, MAX_BYTES).await
}
async fn save_response(
    mut response: reqwest::Response,
    destination: PathBuf,
    overwrite: bool,
    expected: Option<&str>,
    max_bytes: u64,
) -> Result<Value> {
    ensure!(
        response.status() == reqwest::StatusCode::OK,
        "attachment server did not return HTTP 200"
    );
    ensure!(
        response
            .headers()
            .get("content-encoding")
            .is_none_or(|v| v == "identity"),
        "compressed attachment transfer is unsupported"
    );
    let length = response.content_length();
    ensure!(
        length.is_none_or(|n| n <= max_bytes),
        "attachment exceeds byte limit"
    );
    let partial = Partial(
        destination
            .parent()
            .context("missing parent")?
            .join(format!(
                ".dev-session-mcp-import-{}.part",
                uuid::Uuid::new_v4()
            )),
    );
    let mut output = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&partial.0)
        .await?;
    let mut bytes = 0_u64;
    let mut hash = Sha256::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| anyhow::anyhow!("attachment body transfer failed"))?
    {
        bytes = bytes
            .checked_add(chunk.len() as u64)
            .context("attachment size overflow")?;
        ensure!(bytes <= max_bytes, "attachment exceeds byte limit");
        hash.update(&chunk);
        output.write_all(&chunk).await?;
    }
    ensure!(
        length.is_none_or(|n| n == bytes),
        "attachment length mismatch"
    );
    let hash = format!("{:x}", hash.finalize());
    ensure!(
        expected.is_none_or(|e| e.eq_ignore_ascii_case(&hash)),
        "attachment SHA256 mismatch"
    );
    output.sync_all().await?;
    drop(output);
    if overwrite {
        tokio::fs::rename(&partial.0, &destination).await?;
    } else {
        // link is atomic and refuses an existing target; exists()+rename would race.
        tokio::fs::hard_link(&partial.0, &destination).await?;
    }
    Ok(json!({"path":destination,"bytes":bytes,"sha256":hash}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Router, body::Body, http::Response, routing::get};

    #[tokio::test]
    async fn bounded_binary_transfer_publishes_only_verified_complete_files() -> Result<()> {
        let fixture = tempfile::tempdir()?;
        let destination = fixture.path().join("attachment.bin");
        let payload: Vec<u8> = (0_u32..80000).map(|i| (i % 251) as u8).collect();
        let digest = format!("{:x}", Sha256::digest(&payload));
        let bytes = payload.clone();
        let app = Router::new().route(
            "/file",
            get(move || {
                let bytes = bytes.clone();
                async move {
                    Response::builder()
                        .body(Body::from_stream(futures_util::stream::once(async move {
                            Ok::<_, std::io::Error>(bytes)
                        })))
                        .unwrap()
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let url = format!("http://{}/file", listener.local_addr()?);
        let task = tokio::spawn(async move { axum::serve(listener, app).await });
        let client = reqwest::Client::builder().no_proxy().build()?;
        let response = client.get(&url).send().await?;
        ensure!(
            response.content_length().is_none(),
            "fixture must exercise streamed byte limit"
        );
        ensure!(
            save_response(response, destination.clone(), false, None, 1024)
                .await
                .is_err()
        );
        ensure!(!destination.exists() && std::fs::read_dir(fixture.path())?.count() == 0);
        ensure!(
            save_response(
                client.get(&url).send().await?,
                destination.clone(),
                false,
                Some(&"0".repeat(64)),
                MAX_BYTES
            )
            .await
            .is_err()
        );
        ensure!(!destination.exists() && std::fs::read_dir(fixture.path())?.count() == 0);
        let result = save_response(
            client.get(&url).send().await?,
            destination.clone(),
            false,
            Some(&digest),
            MAX_BYTES,
        )
        .await?;
        ensure!(result["bytes"] == 80000 && result["sha256"] == digest);
        ensure!(tokio::fs::read(&destination).await? == payload);
        use std::os::unix::fs::PermissionsExt;
        ensure!(std::fs::metadata(&destination)?.permissions().mode() & 0o777 == 0o600);
        tokio::fs::write(&destination, b"existing user file").await?;
        ensure!(
            save_response(
                client.get(&url).send().await?,
                destination.clone(),
                false,
                None,
                MAX_BYTES
            )
            .await
            .is_err()
        );
        ensure!(tokio::fs::read(&destination).await? == b"existing user file");
        save_response(
            client.get(&url).send().await?,
            destination.clone(),
            true,
            Some(&digest),
            MAX_BYTES,
        )
        .await?;
        ensure!(
            tokio::fs::read(&destination).await? == payload
                && std::fs::read_dir(fixture.path())?.count() == 1
        );
        task.abort();
        println!(
            "PASS actual local HTTP binary stream: 80000 bytes, SHA256, byte cap, failed-transfer cleanup, atomic no-overwrite and explicit replacement; public HTTPS/ChatGPT delivery not exercised"
        );
        Ok(())
    }

    #[test]
    fn attachment_policy_rejects_private_destinations_and_unapproved_origins() -> Result<()> {
        let origins = "https://attachments.example.test";
        ensure!(
            authorized_url(
                "https://attachments.example.test/file?signed=private",
                origins
            )
            .is_ok()
        );
        for url in [
            "http://attachments.example.test/file",
            "https://127.0.0.1/file",
            "https://attachments.example.test.evil.test/file",
            "https://user@attachments.example.test/file",
        ] {
            ensure!(authorized_url(url, origins).is_err());
        }
        for ip in [
            "127.0.0.1",
            "10.0.0.1",
            "100.64.1.2",
            "169.254.169.254",
            "::1",
            "fc00::1",
            "::ffff:127.0.0.1",
            "2001:db8::1",
        ] {
            ensure!(!public_address(ip.parse()?));
        }
        ensure!(public_address("1.1.1.1".parse()?) && public_address("2606:4700::1111".parse()?));
        Ok(())
    }
}
