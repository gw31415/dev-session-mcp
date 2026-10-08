mod base;
use base::{config, sandbox};
mod broker;
mod events;
mod files;
mod server;
mod wait;
mod workspace;

use anyhow::Result;
use clap::{Parser, Subcommand};
#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    /// Credential-cleaned stdio MCP for the authenticated Secure MCP Tunnel.
    Stdio,
    /// Independent same-UID execution owner (normally started automatically).
    Broker,
}
fn main() -> Result<()> {
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
    let command = Cli::parse().command;
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(async {
            match command {
                Command::Stdio => server::stdio().await,
                Command::Broker => broker::run().await,
            }
        })
}
