#[path = "../../vendor/local-mcp/src/approvals.rs"]
mod approvals;
mod auth;
#[path = "../../vendor/local-mcp/src/config.rs"]
mod config;
mod server;
mod workspace;
#[allow(dead_code)]
mod mcp {
    include!(concat!(env!("OUT_DIR"), "/mcp.rs"));
}
mod sandbox {
    include!(concat!(env!("OUT_DIR"), "/sandbox.rs"));
}

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::{os::unix::process::CommandExt, path::PathBuf};

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    /// Retained optional OAuth HTTP mode; unused by the Tunnel deployment.
    Serve {
        #[arg(long)]
        config: PathBuf,
    },
    /// Stdio MCP for the credential-separated Secure MCP Tunnel deployment.
    Stdio,
    /// Local session approval terminal (normally started inside tmux).
    Start { session_id: String },
    #[command(hide = true)]
    Worker { spec: PathBuf },
}

fn main() -> Result<()> {
    // Codex uses this same argv0 when re-entering after bubblewrap.
    if std::env::args_os().next().is_some_and(|v| {
        std::path::Path::new(&v)
            .file_name()
            .is_some_and(|v| v == "codex-linux-sandbox")
    }) {
        codex_linux_sandbox::run_main();
    }
    unsafe {
        libc::umask(0o077);
    }
    match Cli::parse().command {
        Command::Worker { spec } => {
            let spec: workspace::WorkerSpec = serde_json::from_slice(&std::fs::read(spec)?)?;
            let (program, args) = spec.command.split_first().context("empty command")?;
            let error = std::process::Command::new(program)
                .args(args)
                .current_dir(spec.cwd)
                .env_clear()
                .envs(spec.env)
                .exec();
            // Do not print argv, environment, or user command contents.
            eprintln!("Could not start command: {}", error.kind());
            std::process::exit(127);
        }
        command => tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?
            .block_on(async {
                match command {
                    Command::Serve { config } => server::http(config).await,
                    Command::Stdio => server::stdio().await,
                    Command::Start { session_id } => approvals::start(Some(&session_id)).await,
                    Command::Worker { .. } => unreachable!(),
                }
            }),
    }
}
