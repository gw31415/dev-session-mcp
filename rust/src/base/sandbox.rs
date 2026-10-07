// Derived from nakasyou/local-mcp. Copyright (c) 2026 Shotaro Nakamura.
// Adapted and maintained by dev-session-mcp; see NOTICE.md and docs/UPSTREAM.md.
// Upstream MIT notice: licenses/local-mcp-MIT.txt.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use codex_protocol::models::PermissionProfile;
use codex_protocol::permissions::NetworkSandboxPolicy;
use codex_utils_absolute_path::AbsolutePathBuf;
use tokio::process::Command;

fn absolute(path: &Path) -> Result<AbsolutePathBuf> {
    let path = if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir()?.join(path)
    };
    AbsolutePathBuf::from_absolute_path(path).map_err(|error| anyhow::anyhow!(error))
}

pub fn command(command: &[String], cwd: &Path, writable_roots: &[PathBuf]) -> Result<Command> {
    anyhow::ensure!(!command.is_empty(), "command must not be empty");
    let cwd = std::fs::canonicalize(cwd)
        .with_context(|| format!("cannot resolve cwd {}", cwd.display()))?;
    let roots = writable_roots
        .iter()
        .map(|path| absolute(path))
        .collect::<Result<Vec<_>>>()?;
    let permissions = PermissionProfile::workspace_write_with(
        &roots,
        NetworkSandboxPolicy::Restricted,
        true,
        true,
    )
    .materialize_project_roots_with_workspace_roots(&[absolute(&cwd)?]);

    #[cfg(target_os = "linux")]
    let mut process = {
        let args =
            codex_sandboxing::landlock::create_linux_sandbox_command_args_for_permission_profile(
                command.to_vec(),
                &cwd,
                &permissions,
                &cwd,
                false,
                false,
            );
        let executable = std::env::current_exe()?;
        anyhow::ensure!(
            executable.is_file(),
            "sandbox helper is missing: {}",
            executable.display()
        );
        let mut process = Command::new(executable);
        process.arg0(codex_sandboxing::landlock::CODEX_LINUX_SANDBOX_ARG0);
        process.args(args);
        process
    };

    #[cfg(target_os = "macos")]
    let mut process = {
        use codex_sandboxing::seatbelt::CreateSeatbeltCommandArgsParams;
        use codex_sandboxing::seatbelt::MACOS_PATH_TO_SEATBELT_EXECUTABLE;
        use codex_sandboxing::seatbelt::create_seatbelt_command_args;

        let (file_system_policy, network_policy) = permissions.to_runtime_permissions();
        let args = create_seatbelt_command_args(CreateSeatbeltCommandArgsParams {
            command: command.to_vec(),
            file_system_sandbox_policy: &file_system_policy,
            network_sandbox_policy: network_policy,
            sandbox_policy_cwd: &cwd,
            enforce_managed_network: false,
            network: None,
            extra_allow_unix_sockets: &[],
        });
        let mut process = Command::new(MACOS_PATH_TO_SEATBELT_EXECUTABLE);
        process.args(args);
        process
    };

    #[cfg(windows)]
    let mut process = {
        // Windows has no equivalent of Landlock/Seatbelt in this application.
        // Preserve argv execution and the restricted environment so the
        // feature remains usable, while documenting that this is not a
        // filesystem/network sandbox.
        let mut process = Command::new(&command[0]);
        process.args(&command[1..]);
        process
    };

    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    let mut process = { anyhow::bail!("sandboxed execution is unsupported on this platform") };

    process
        .current_dir(&cwd)
        .env_clear()
        .envs(crate::workspace::safe_environment());
    Ok(process)
}
