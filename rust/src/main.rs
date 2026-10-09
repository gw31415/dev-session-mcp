mod base;
use base::{config, sandbox};
mod broker;
mod durable;
mod events;
mod files;
mod http;
mod profiles;
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
    /// MCP over stdio (local clients, SSH, tunnel-client). Any number may run at once.
    Stdio,
    /// MCP over Streamable HTTP at /mcp. No built-in authentication.
    Http {
        /// Address to listen on. Non-loopback requires --allow-remote-bind.
        #[arg(long, default_value = "127.0.0.1:8808")]
        listen: std::net::SocketAddr,
        /// Extra accepted Host header values, e.g. the public proxy hostname.
        #[arg(long = "allowed-host", value_delimiter = ',')]
        allowed_hosts: Vec<String>,
        /// Accepted browser Origin values (requests with any other Origin are rejected).
        #[arg(long = "allowed-origin", value_delimiter = ',')]
        allowed_origins: Vec<String>,
        /// Allow a non-loopback listen address (only behind an authenticating proxy).
        #[arg(long)]
        allow_remote_bind: bool,
    },
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
                Command::Http {
                    listen,
                    allowed_hosts,
                    allowed_origins,
                    allow_remote_bind,
                } => {
                    http::serve(http::Options {
                        listen,
                        allowed_hosts,
                        allowed_origins,
                        allow_remote_bind,
                    })
                    .await
                }
                Command::Broker => broker::run().await,
            }
        })
}
