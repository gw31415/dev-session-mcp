use anyhow::{Context, Result, ensure};
use serde_json::Value;
use std::{collections::HashMap, path::PathBuf};

pub const MAX_OUTPUT: usize = 65536;
pub fn state_dir() -> Result<PathBuf> {
    Ok(std::env::var_os("DEV_SESSION_MCP_STATE_DIR")
        .map(PathBuf::from)
        .unwrap_or(
            dirs::home_dir()
                .context("HOME is required")?
                .join(".local/state/dev-session-mcp"),
        ))
}
pub fn clean_environment() -> Result<()> {
    ensure!(
        !std::env::vars_os().any(|(k, _)| {
            let k = k.to_string_lossy();
            k.starts_with("CONTROL_PLANE_")
                || k.starts_with("TUNNEL_")
                || k.starts_with("MCP_")
                || k == "OPENAI_ADMIN_KEY"
                || k == "CREDENTIALS_DIRECTORY"
        }),
        "transport environment detected; use the fixed clean-env launcher"
    );
    Ok(())
}
pub fn safe_environment() -> HashMap<String, String> {
    let mut env = HashMap::from([
        (
            "PATH".into(),
            std::env::var("PATH").unwrap_or("/usr/local/bin:/usr/bin:/bin".into()),
        ),
        (
            "HOME".into(),
            dirs::home_dir()
                .unwrap_or(PathBuf::from("/tmp"))
                .display()
                .to_string(),
        ),
        ("LANG".into(), "C.UTF-8".into()),
        ("TERM".into(), "xterm-256color".into()),
    ]);
    for key in ["USER", "LOGNAME", "SHELL", "TMPDIR", "XDG_STATE_HOME"] {
        if let Ok(value) = std::env::var(key) {
            env.insert(key.into(), value);
        }
    }
    env
}
pub fn text<'a>(args: &'a Value, key: &str) -> Result<&'a str> {
    let value = args[key]
        .as_str()
        .with_context(|| format!("missing string {key}"))?;
    ensure!(
        value.len() <= MAX_OUTPUT && !value.contains('\0'),
        "invalid or oversized string"
    );
    Ok(value)
}
pub fn bounded(value: &str, limit: usize, _: bool) -> (String, bool) {
    let mut end = value.len().min(limit);
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    (value[..end].into(), end < value.len())
}
