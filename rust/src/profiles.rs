//! Operator-owned bubblewrap definitions. Never accept configuration/argv from MCP.
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    io::Read,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
};

pub const ENV: &str = "DEV_SESSION_MCP_PROFILES";
const LIMIT: u64 = 65536;
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    version: u32,
    profiles: Vec<Definition>,
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Definition {
    id: String,
    bwrap: PathBuf,
    args: Vec<String>,
}

pub fn custom(name: &str) -> bool {
    name.starts_with("admin:")
}
fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

// This is the complete non-FD option grammar supported by bubblewrap 0.9, not a
// security allowlist. Administrators control mounts, capabilities and networking.
fn arity(option: &str) -> Result<usize> {
    Ok(match option {
        "--unshare-all"
        | "--share-net"
        | "--unshare-user"
        | "--unshare-user-try"
        | "--unshare-ipc"
        | "--unshare-pid"
        | "--unshare-net"
        | "--unshare-uts"
        | "--unshare-cgroup"
        | "--unshare-cgroup-try"
        | "--disable-userns"
        | "--assert-userns-disabled"
        | "--clearenv"
        | "--new-session"
        | "--die-with-parent"
        | "--as-pid-1" => 0,
        "--argv0" | "--uid" | "--gid" | "--hostname" | "--chdir" | "--unsetenv" | "--lock-file"
        | "--remount-ro" | "--exec-label" | "--file-label" | "--proc" | "--dev" | "--tmpfs"
        | "--mqueue" | "--dir" | "--cap-add" | "--cap-drop" | "--perms" | "--size" => 1,
        "--setenv" | "--bind" | "--bind-try" | "--dev-bind" | "--dev-bind-try" | "--ro-bind"
        | "--ro-bind-try" | "--symlink" | "--chmod" => 2,
        "--args" | "--userns" | "--userns2" | "--pidns" | "--sync-fd" | "--bind-fd"
        | "--ro-bind-fd" | "--file" | "--bind-data" | "--ro-bind-data" | "--seccomp"
        | "--add-seccomp-fd" | "--block-fd" | "--userns-block-fd" | "--info-fd"
        | "--json-status-fd" => anyhow::bail!("admin profile FD options are unsupported"),
        _ => anyhow::bail!("admin profile has an unknown or reserved option"),
    })
}
fn validate_args(args: &[String]) -> Result<()> {
    ensure!(
        args.len() <= 512 && args.iter().all(|s| s.len() <= 4096 && !s.contains('\0')),
        "admin profile argv exceeds limits"
    );
    let mut i = 0;
    while i < args.len() {
        let n = arity(&args[i])?;
        ensure!(i + n < args.len(), "admin profile option operand missing");
        i += n + 1;
    }
    Ok(())
}
impl Definition {
    fn expand(&self, cwd: &Path, session: &Path, roots: &[PathBuf]) -> Result<Vec<String>> {
        let path = |p: &Path| -> Result<String> {
            p.to_str()
                .map(str::to_owned)
                .ok_or_else(|| anyhow::anyhow!("admin profile requires UTF-8 paths"))
        };
        let mut out = Vec::new();
        for arg in &self.args {
            match arg.as_str() {
                "{{cwd}}" => out.push(path(cwd)?),
                "{{session_cwd}}" => out.push(path(session)?),
                "{{writable_roots}}" => {
                    for root in roots {
                        out.extend(["--bind".into(), path(root)?, path(root)?]);
                    }
                }
                other => {
                    ensure!(
                        !other.contains("{{") && !other.contains("}}"),
                        "invalid admin profile placeholder"
                    );
                    out.push(other.to_owned());
                }
            }
        }
        validate_args(&out)?;
        Ok(out)
    }
    pub fn metadata(&self) -> Result<Value> {
        Ok(
            json!({"id":format!("admin:{}",self.id),"definition_sha256":crate::durable::digest(&json!(self))?,
            "backend":"bubblewrap-direct","security":"administrator-defined","codex_inner_seccomp":false}),
        )
    }
    pub fn command(
        &self,
        command: &[String],
        cwd: &Path,
        session: &Path,
        roots: &[PathBuf],
    ) -> Result<tokio::process::Command> {
        let executable = std::fs::metadata(&self.bwrap)
            .map_err(|_| anyhow::anyhow!("admin bubblewrap executable unavailable"))?;
        ensure!(
            executable.is_file() && executable.mode() & 0o111 != 0,
            "admin bubblewrap executable must be an executable regular file"
        );
        let args = self.expand(cwd, session, roots)?;
        let mut p = tokio::process::Command::new(&self.bwrap);
        p.args(args)
            .arg("--")
            .args(command)
            .current_dir(cwd)
            .env_clear()
            .envs(crate::workspace::safe_environment());
        Ok(p)
    }
}
fn parse(bytes: &[u8]) -> Result<Vec<Definition>> {
    // Never propagate serde errors: an unknown field or value can contain a secret.
    let config: Config = serde_json::from_slice(bytes)
        .map_err(|_| anyhow::anyhow!("invalid admin profile configuration"))?;
    ensure!(
        config.version == 1 && config.profiles.len() <= 32,
        "unsupported admin profile configuration version or count"
    );
    let mut ids = BTreeSet::new();
    for def in &config.profiles {
        ensure!(
            valid_id(&def.id) && ids.insert(&def.id),
            "invalid or duplicate admin profile ID"
        );
        ensure!(
            def.bwrap.is_absolute()
                && def
                    .bwrap
                    .to_str()
                    .is_some_and(|s| s.len() <= 4096 && !s.contains('\0') && !s.contains("{{")),
            "admin bwrap executable must be an absolute path"
        );
        ensure!(def.args.len() <= 512, "admin profile argv exceeds limits");
        def.expand(
            Path::new("/cwd"),
            Path::new("/session"),
            &[PathBuf::from("/root")],
        )?;
    }
    Ok(config.profiles)
}
fn load() -> Result<Vec<Definition>> {
    let explicit = std::env::var_os(ENV);
    let path = explicit
        .clone()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/etc/dev-session-mcp/profiles.json"));
    ensure!(
        path.is_absolute(),
        "admin profile configuration path must be absolute"
    );
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path);
    let mut file = match file {
        Ok(file) => file,
        Err(e) if explicit.is_none() && e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Vec::new());
        }
        Err(_) => anyhow::bail!("admin profile configuration unavailable"),
    };
    let meta = file
        .metadata()
        .map_err(|_| anyhow::anyhow!("admin profile configuration unreadable"))?;
    ensure!(
        meta.is_file()
            && meta.len() <= LIMIT
            && meta.mode() & 0o022 == 0
            && (meta.uid() == 0 || meta.uid() == unsafe { libc::geteuid() }),
        "admin profile configuration must be bounded, regular, owned by root or broker UID and not group/other writable"
    );
    let mut bytes = Vec::new();
    (&mut file)
        .take(LIMIT + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| anyhow::anyhow!("admin profile configuration unreadable"))?;
    ensure!(
        bytes.len() <= LIMIT as usize,
        "admin profile configuration oversized"
    );
    parse(&bytes)
}
pub fn resolve(name: &str) -> Result<Definition> {
    ensure!(
        cfg!(target_os = "linux"),
        "admin bubblewrap profiles require Linux"
    );
    ensure!(
        name.strip_prefix("admin:").is_some_and(valid_id),
        "invalid admin profile ID"
    );
    load()?
        .into_iter()
        .find(|d| name == format!("admin:{}", d.id))
        .ok_or_else(|| anyhow::anyhow!("admin profile unavailable"))
}
pub fn catalog() -> Value {
    match load().and_then(|defs| {
        defs.iter()
            .map(Definition::metadata)
            .collect::<Result<Vec<_>>>()
    }) {
        Ok(profiles) => {
            json!({"version":1,"available":cfg!(target_os="linux"),"profiles":profiles})
        }
        Err(_) => {
            json!({"version":1,"available":false,"profiles":[],"error":"admin profile configuration invalid or unavailable"})
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn definition(args: &[&str]) -> Definition {
        Definition {
            id: "test".into(),
            bwrap: "/usr/bin/bwrap".into(),
            args: args.iter().map(|s| s.to_string()).collect(),
        }
    }
    #[test]
    fn general_policy_and_command_boundary() {
        let mut d = definition(&[
            "--ro-bind",
            "/",
            "/",
            "--unshare-all",
            "--share-net",
            "--cap-drop",
            "ALL",
            "--setenv",
            "LABEL",
            "--",
            "{{writable_roots}}",
            "--chdir",
            "{{cwd}}",
        ]);
        d.bwrap = "/bin/true".into(); // argv construction only; no dependency on installed bwrap
        let command = vec![
            "/bin/printf".into(),
            "--unshare-net".into(),
            "$(not-a-shell)".into(),
        ];
        let p = d
            .command(
                &command,
                Path::new("/a space"),
                Path::new("/session"),
                &[PathBuf::from("/a space")],
            )
            .unwrap();
        let args: Vec<_> = p.as_std().get_args().map(|s| s.to_str().unwrap()).collect();
        assert_eq!(
            &args[args.len() - 4..],
            &["--", "/bin/printf", "--unshare-net", "$(not-a-shell)"]
        );
        assert!(
            args.windows(3)
                .any(|v| v == ["--bind", "/a space", "/a space"])
        );
        assert!(!args.contains(&"--apply-seccomp-then-exec"));
    }
    #[test]
    fn reject_fd_argv_injection_and_unknown_templates() {
        for args in [
            vec!["--args", "0"],
            vec!["--seccomp", "0"],
            vec!["--"],
            vec!["/bin/sh"],
            vec!["--future-option"],
            vec!["--bind", "/"],
            vec!["--chdir", "{{secret}}"],
            vec!["--chdir={{cwd}}"],
        ] {
            assert!(
                definition(&args)
                    .expand(Path::new("/a"), Path::new("/a"), &[])
                    .is_err(),
                "{args:?}"
            );
        }
    }
    #[test]
    fn metadata_is_redacted_and_changes_with_policy() {
        let mut d = definition(&["--setenv", "PRIVATE", "CANARY_PRIVATE"]);
        let old = d.metadata().unwrap();
        assert!(!old.to_string().contains("CANARY_PRIVATE"));
        d.args[2] = "changed".into();
        assert_ne!(
            old["definition_sha256"],
            d.metadata().unwrap()["definition_sha256"]
        );
        let error = parse(br#"{"version":1,"profiles":[],"CANARY_PRIVATE":true}"#)
            .err()
            .unwrap()
            .to_string();
        assert!(!error.contains("CANARY_PRIVATE"));
        assert!(
            parse(br#"{"version":1,"profiles":[{"id":"x","bwrap":"relative","args":[]}]}"#)
                .is_err()
        );
        assert!(parse(br#"{"version":1,"profiles":[{"id":"x","bwrap":"/b","args":[]},{"id":"x","bwrap":"/b","args":[]}]}"#).is_err());
    }
}
