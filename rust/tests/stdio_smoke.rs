mod common;

use anyhow::{Context, Result, ensure};
use common::{Client, call, raw, result_text, until};
use rmcp::{ServiceExt, model::ClientConfig, transport::TokioChildProcess};
use serde_json::json;
use std::{collections::HashMap, path::PathBuf, process::Stdio, time::Duration};
use tokio::process::Command;

// This fixture does not start HTTP or an OAuth server and has no Tunnel key.
struct StdioFixture {
    home: tempfile::TempDir,
    cwd: PathBuf,
    binary: PathBuf,
    env: HashMap<String, String>,
}

impl StdioFixture {
    async fn new() -> Result<Self> {
        let home = tempfile::Builder::new()
            .prefix("odm-stdio-")
            .tempdir_in("/tmp")?;
        let cwd = home.path().join("project");
        tokio::fs::create_dir(&cwd).await?;
        let env = HashMap::from([
            ("PATH".into(), "/usr/bin:/bin".into()),
            ("HOME".into(), home.path().display().to_string()),
            ("LANG".into(), "C.UTF-8".into()),
            (
                "XDG_STATE_HOME".into(),
                home.path().join("xdg").display().to_string(),
            ),
            (
                "OCI_DEV_STATE_DIR".into(),
                home.path().join("state").display().to_string(),
            ),
            (
                "OCI_DEV_TMUX_BIN".into(),
                std::env::var("OCI_DEV_TMUX_BIN").unwrap_or("tmux".into()),
            ),
        ]);
        let binary = std::env::var_os("OCI_DEV_RUST_BIN")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_oci-dev-mcp")));
        Ok(Self {
            home,
            cwd,
            binary,
            env,
        })
    }
    fn command(&self) -> Command {
        let mut command = Command::new(&self.binary);
        command.arg("stdio").env_clear().envs(&self.env);
        command
    }
    async fn connect(&self) -> Result<(Client, u32)> {
        self.connect_command(self.command()).await
    }
    async fn connect_command(&self, command: Command) -> Result<(Client, u32)> {
        let log = self
            .home
            .path()
            .join(format!("stderr-{}.log", uuid::Uuid::new_v4()));
        let (transport, _) = TokioChildProcess::builder(command)
            .stderr(std::fs::File::create(log)?)
            .spawn()?;
        let pid = transport.id().context("missing Rust stdio process ID")?;
        let client = ClientConfig::default().serve(transport).await?;
        Ok((client, pid))
    }
    async fn wait_exit(pid: u32) -> Result<()> {
        for _ in 0..100 {
            if !std::path::Path::new(&format!("/proc/{pid}")).exists() {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        anyhow::bail!("Rust stdio process did not exit after disconnect");
    }
}
impl Drop for StdioFixture {
    fn drop(&mut self) {
        let _ = std::process::Command::new(&self.env["OCI_DEV_TMUX_BIN"])
            .args(["-S"])
            .arg(self.home.path().join("state/tmux.sock"))
            .arg("kill-server")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rust_stdio_tools_and_reconnect_without_node_or_oauth() -> Result<()> {
    let fixture = StdioFixture::new().await?;
    let (client, first_pid) = fixture.connect().await?;
    ensure!(client.list_tools(None).await?.tools.len() == 20);
    println!("PASS actual Rust stdio and official Rust SDK: 20 tools; no Node/HTTP/OAuth fixture");

    let sid = "stdio-check";
    call(
        &client,
        "create_session",
        json!({"session_id":sid,"cwd":fixture.cwd}),
    )
    .await?;
    call(
        &client,
        "set_memo",
        json!({"session_id":sid,"text":"Purpose: Tunnel stdio\nNext: reconnect"}),
    )
    .await?;
    let result = raw(
        &client,
        "execute",
        json!({"session_id":sid,"command":["/bin/sh","-c","printf STDIO_EXEC_OK"]}),
    )
    .await?;
    ensure!(
        result.is_error != Some(true) && result_text(&result)?.contains("STDIO_EXEC_OK"),
        "stdio sandbox execute failed"
    );
    for content in ["before\n", "after\n"] {
        ensure!(
            raw(
                &client,
                "write_file",
                json!({"session_id":sid,"path":"edit.txt","content":content})
            )
            .await?
            .is_error
                != Some(true)
        );
    }
    ensure!(
        result_text(
            &raw(
                &client,
                "read_file",
                json!({"session_id":sid,"path":"edit.txt"})
            )
            .await?
        )? == "after\n"
    );
    println!("PASS stdio session/memo and real sandbox command/file read-write-edit");

    let job = call(
        &client,
        "mux_open",
        json!({"session_id":sid,"command":["/bin/bash","-c","printf STDIO_READY; read value; printf 'STDIN:%s\n' \"$value\"; sleep 30"]}),
    )
    .await?;
    let args = json!({"session_id":sid,"job_id":job["job_id"]});
    until(&client, args.clone(), |r| {
        r["output"].as_str().unwrap().contains("STDIO_READY")
    })
    .await?;
    client.cancel().await?;
    StdioFixture::wait_exit(first_pid).await?;

    let (client, second_pid) = fixture.connect().await?;
    ensure!(second_pid != first_pid, "stdio child was not replaced");
    let sessions = call(&client, "list_sessions", json!({})).await?;
    ensure!(sessions["sessions"].as_array().unwrap().len() == 1);
    let info = call(&client, "connect_session", json!({"session_id":sid})).await?;
    ensure!(
        info["mux_jobs"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v["job_id"] == job["job_id"])
    );
    ensure!(
        call(&client, "get_memo", json!({"session_id":sid})).await?["text"]
            .as_str()
            .unwrap()
            .contains("reconnect")
    );
    call(
        &client,
        "mux_send",
        json!({"session_id":sid,"job_id":job["job_id"],"text":"hello","keys":["Enter"]}),
    )
    .await?;
    ensure!(
        until(&client, args.clone(), |r| {
            r["output"].as_str().unwrap().contains("STDIN:hello")
        })
        .await?["status"]
            == "running"
    );
    call(&client, "mux_stop", args.clone()).await?;
    ensure!(call(&client, "mux_poll", args).await?["status"] == "unavailable");
    println!(
        "PASS Rust stdio actual exit/replacement, session/memo/tmux reconnect, live stdin and stop"
    );

    let job = call(
        &client,
        "mux_open",
        json!({"session_id":sid,"command":["/bin/bash","-c","for i in {1..300}; do printf '%0100d\n' \"$i\"; done; exit 7"]}),
    )
    .await?;
    let capped = until(
        &client,
        json!({"session_id":sid,"job_id":job["job_id"],"max_output_bytes":1024}),
        |r| r["status"] == "completed",
    )
    .await?;
    ensure!(capped["output_bytes"].as_u64().unwrap() <= 1024);
    ensure!(capped["truncated"] == true && capped["exit_code"] == 7);
    let job = call(
        &client,
        "mux_open",
        json!({"session_id":sid,"command":["/usr/bin/env"]}),
    )
    .await?;
    let environment = until(
        &client,
        json!({"session_id":sid,"job_id":job["job_id"]}),
        |r| r["status"] == "completed",
    )
    .await?;
    let output = environment["output"].as_str().unwrap();
    ensure!(
        !["TOKEN", "CREDENTIAL", "CONTROL_PLANE", "TUNNEL_", "MCP_"]
            .iter()
            .any(|marker| output.contains(marker))
    );
    call(&client, "close_session", json!({"session_id":sid})).await?;
    ensure!(
        call(&client, "list_sessions", json!({})).await?["sessions"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    client.cancel().await?;
    StdioFixture::wait_exit(second_pid).await?;

    let fake_key = "fixture-not-a-Tunnel-runtime-key";
    let output = fixture
        .command()
        .env("CONTROL_PLANE_API_KEY", fake_key)
        .output()
        .await?;
    ensure!(!output.status.success(), "transport environment accepted");
    ensure!(!String::from_utf8_lossy(&output.stderr).contains(fake_key));
    println!(
        "PASS stdio output cap/exit, clean shell environment, explicit close and transport-env rejection"
    );

    // Exercise the real launcher body with fixture paths, without OS users/sudo
    // installation. UID separation is a deployment boundary, not simulated here.
    let shell_path = |value: &std::path::Path| {
        format!("'{}'", value.display().to_string().replace('\'', "'\\''"))
    };
    let tmux_dir = PathBuf::from(&fixture.env["OCI_DEV_TMUX_BIN"])
        .parent()
        .context("tmux path needs a directory")?
        .to_owned();
    let launcher = include_str!("../../deploy/oci-dev-rust-stdio")
        .replace("/home/devmcp/projects", &shell_path(&fixture.cwd))
        .replace("/home/devmcp", &shell_path(fixture.home.path()))
        .replace(
            "/opt/oci-dev-mcp/bin/oci-dev-mcp",
            &shell_path(&fixture.binary),
        )
        .replace(
            "PATH=/usr/local/bin:/usr/bin:/bin",
            &format!("PATH='{}:/usr/bin:/bin'", tmux_dir.display()),
        );
    let path = fixture.home.path().join("launcher.sh");
    tokio::fs::write(&path, launcher).await?;
    let mut command = Command::new("/bin/sh");
    command
        .arg(path)
        .env_clear()
        .envs(&fixture.env)
        .env("CONTROL_PLANE_API_KEY", fake_key)
        .env("CREDENTIALS_DIRECTORY", "/fixture-only-credential-dir");
    let (client, pid) = fixture.connect_command(command).await?;
    ensure!(client.list_tools(None).await?.tools.len() == 20);
    client.cancel().await?;
    StdioFixture::wait_exit(pid).await?;
    println!(
        "PASS real clean-env launcher body starts Rust stdio with incoming fake Tunnel credentials removed; UID transition not tested"
    );
    Ok(())
}
