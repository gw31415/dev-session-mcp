// Derived from nakasyou/local-mcp. Copyright (c) 2026 Shotaro Nakamura.
// Adapted and maintained by dev-session-mcp; see NOTICE.md and docs/UPSTREAM.md.
// Upstream MIT notice: licenses/local-mcp-MIT.txt.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub cwd: PathBuf,
    #[serde(default)]
    pub permitted_directories: Vec<PathBuf>,
}

pub fn state_dir() -> Result<PathBuf> {
    crate::workspace::state_dir()
}

pub fn session_path(id: &str) -> Result<PathBuf> {
    validate_session_id(id)?;
    Ok(state_dir()?.join("sessions").join(format!("{id}.json")))
}

pub async fn create_session(cwd: &Path, id: Option<&str>) -> Result<Session> {
    let cwd = canonical_directory(cwd)?;
    let id = id.map(str::to_owned).unwrap_or_else(|| {
        format!("{:x}", Sha256::digest(cwd.as_os_str().as_encoded_bytes()))[..24].to_owned()
    });
    validate_session_id(&id)?;
    if let Ok(existing) = load_session(&id).await {
        return Ok(existing);
    }
    let mut entries = tokio::fs::read_dir(state_dir()?.join("sessions")).await?;
    let mut count = 0;
    while let Some(entry) = entries.next_entry().await? {
        if entry.path().extension().is_some_and(|s| s == "json") {
            count += 1;
        }
    }
    anyhow::ensure!(
        count < 64,
        "64 project sessions reached; close one before opening another"
    );
    let session = Session {
        id,
        cwd: cwd.clone(),
        permitted_directories: vec![cwd],
    };
    save_session(&session).await?;
    Ok(session)
}

pub async fn load_session(id: &str) -> Result<Session> {
    let path = session_path(id)?;
    let bytes = tokio::fs::read(&path)
        .await
        .with_context(|| format!("session {id} was not found; use open_session first"))?;
    serde_json::from_slice(&bytes).context("invalid session")
}

pub async fn save_session(session: &Session) -> Result<()> {
    let path = session_path(&session.id)?;
    tokio::fs::create_dir_all(path.parent().unwrap()).await?;
    let temporary = path.with_extension(format!("json.{}.tmp", std::process::id()));
    tokio::fs::write(&temporary, serde_json::to_vec_pretty(session)?).await?;
    tokio::fs::rename(temporary, path).await?;
    Ok(())
}

pub fn validate_session_id(id: &str) -> Result<()> {
    anyhow::ensure!(!id.is_empty(), "session ID must not be empty");
    anyhow::ensure!(id.len() <= 64, "session ID must be at most 64 bytes");
    anyhow::ensure!(
        id.chars()
            .all(|character| character.is_ascii_alphanumeric() || "-_.".contains(character)),
        "session ID may contain only ASCII letters, numbers, '-', '_', and '.'"
    );
    anyhow::ensure!(id != "." && id != "..", "invalid session ID");
    Ok(())
}

pub fn canonical_directory(path: &Path) -> Result<PathBuf> {
    let path = std::fs::canonicalize(path)
        .with_context(|| format!("cannot resolve {}", path.display()))?;
    anyhow::ensure!(path.is_dir(), "{} is not a directory", path.display());
    Ok(path)
}
