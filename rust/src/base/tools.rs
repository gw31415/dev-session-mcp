// Derived from nakasyou/local-mcp. Copyright (c) 2026 Shotaro Nakamura.
// Adapted and maintained by dev-session-mcp; see NOTICE.md and docs/UPSTREAM.md.
// Upstream MIT notice: licenses/local-mcp-MIT.txt.
use crate::{
    config, sandbox,
    workspace::{MAX_OUTPUT, text},
};
use anyhow::{Context, Result, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    process::Stdio,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn path(cwd: &Path, args: &Value) -> Result<PathBuf> {
    let value = PathBuf::from(text(args, "path")?);
    Ok(if value.is_absolute() {
        value
    } else {
        cwd.join(value)
    })
}
pub async fn call(name: &str, args: &Value) -> Result<Value> {
    let session = config::load_session(text(args, "session_id")?).await?;
    let target = path(&session.cwd, args)?;
    match name {
        "read_file" => {
            let mut bytes = Vec::new();
            tokio::fs::File::open(&target)
                .await?
                .take((MAX_OUTPUT + 1) as u64)
                .read_to_end(&mut bytes)
                .await?;
            let truncated = bytes.len() > MAX_OUTPUT;
            bytes.truncate(MAX_OUTPUT);
            Ok(json!({"path":target,"text":String::from_utf8_lossy(&bytes),"truncated":truncated}))
        }
        "list_directory" => {
            let mut directory = tokio::fs::read_dir(&target).await?;
            let mut entries = Vec::new();
            let mut budget = 0;
            let mut truncated = false;
            while let Some(entry) = directory.next_entry().await? {
                let name = entry.file_name().to_string_lossy().into_owned();
                if entries.len() >= 256 || budget + name.len() > 16384 {
                    truncated = true;
                    break;
                }
                budget += name.len();
                entries.push(json!({"name":name,"directory":entry.file_type().await?.is_dir()}));
            }
            Ok(json!({"entries":entries,"truncated":truncated}))
        }
        "get_image" => {
            let mut bytes = Vec::new();
            tokio::fs::File::open(&target)
                .await?
                .take(1024 * 1024 + 1)
                .read_to_end(&mut bytes)
                .await?;
            ensure!(
                bytes.len() <= 1024 * 1024,
                "image exceeds 1 MiB; resize it first"
            );
            let mime = match target
                .extension()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_lowercase()
                .as_str()
            {
                "png" => "image/png",
                "jpg" | "jpeg" => "image/jpeg",
                "gif" => "image/gif",
                "webp" => "image/webp",
                _ => anyhow::bail!("supported images: PNG, JPEG, GIF, WebP"),
            };
            Ok(json!({"content":[{"type":"image","data":STANDARD.encode(bytes),"mimeType":mime}]}))
        }
        "write_file" => {
            ensure!(
                session.state == config::SessionState::Open,
                "session is closing"
            );
            let content = text(args, "content")?;
            let parent = std::fs::canonicalize(target.parent().context("missing parent")?)?;
            let destination = parent.join(target.file_name().context("missing file name")?);
            let allowed = session
                .permitted_directories
                .iter()
                .any(|root| parent.starts_with(root));
            ensure!(
                allowed,
                "write destination is outside the session's permitted roots"
            );
            let command = vec![
                "sh".into(),
                "-c".into(),
                "cat > \"$1\"".into(),
                "dev-session-mcp-write".into(),
                destination.display().to_string(),
            ];
            let mut process =
                sandbox::command(&command, &session.cwd, &session.permitted_directories)?;
            let mut child = process
                .kill_on_drop(true)
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()?;
            child
                .stdin
                .take()
                .context("missing write stdin")?
                .write_all(content.as_bytes())
                .await?;
            let status =
                tokio::time::timeout(std::time::Duration::from_secs(10), child.wait()).await??;
            ensure!(status.success(), "sandboxed file write failed");
            Ok(json!({"path":destination,"bytes":content.len()}))
        }
        _ => anyhow::bail!("unknown file operation"),
    }
}
