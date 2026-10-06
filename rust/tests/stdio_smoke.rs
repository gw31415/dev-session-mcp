mod common;

use anyhow::{Context, Result, ensure};
use common::{Client, call, raw, result_text, until};
use rmcp::{ServiceExt, model::ClientConfig, transport::TokioChildProcess};
use serde_json::json;
use std::{collections::HashMap, path::PathBuf, time::Duration};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
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
            .prefix("dsm-stdio-")
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
                "DEV_SESSION_MCP_STATE_DIR".into(),
                home.path().join("state").display().to_string(),
            ),
        ]);
        let binary = std::env::var_os("DEV_SESSION_MCP_RUST_BIN")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_dev-session-mcp")));
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
        common::shutdown_backend(&self.home.path().join("state"));
        common::shutdown_backend(&self.home.path().join(".local/state/dev-session-mcp"));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rust_stdio_tools_and_reconnect_without_node_or_oauth() -> Result<()> {
    let fixture = StdioFixture::new().await?;
    let (client, first_pid) = fixture.connect().await?;
    ensure!(
        client
            .peer_info()
            .context("missing initialize result")?
            .server_info
            .as_ref()
            .context("missing MCP server identity")?
            .name
            == "dev-session-mcp"
    );
    ensure!(client.list_tools(None).await?.tools.len() == 20);
    let tools = client.list_tools(None).await?.tools;
    for name in [
        "session_info",
        "read_file",
        "get_image",
        "list_directory",
        "write_file",
        "execute",
        "start_command",
        "poll_job",
        "stop_job",
        "without_sandbox",
    ] {
        ensure!(
            tools.iter().any(|tool| tool.name == name),
            "missing upstream tool {name}"
        );
    }
    ensure!(
        !tools
            .iter()
            .any(|tool| ["get_memo", "set_memo"].contains(&tool.name.as_ref()))
    );
    let descriptor = serde_json::to_value(
        tools
            .iter()
            .find(|t| t.name == "import_file")
            .context("missing import_file")?,
    )?;
    ensure!(descriptor["_meta"]["openai/fileParams"] == json!(["file"]));
    let file_schema = &descriptor["inputSchema"]["properties"]["file"];
    ensure!(file_schema["required"] == json!(["download_url", "file_id"]));
    for field in ["download_url", "file_id", "mime_type", "file_name"] {
        ensure!(file_schema["properties"].get(field).is_some());
    }
    println!(
        "PASS actual dev-session-mcp stdio identity and official Rust SDK: 20 tools; no Node/HTTP/OAuth fixture"
    );

    let sid = "stdio-check";
    let created = call(
        &client,
        "create_session",
        json!({"session_id":sid,"cwd":fixture.cwd}),
    )
    .await?;
    let blocked = raw(&client, "import_file", json!({"session_id":sid,"file":{"download_url":"https://unapproved.example.test/file?signature=PRIVATE_CAPABILITY","file_id":"file_fixture"},"path":"blocked.bin"})).await?;
    let blocked_text = result_text(&blocked)?;
    ensure!(
        blocked.is_error == Some(true)
            && blocked_text.contains("origin not configured or permitted")
            && !blocked_text.contains("PRIVATE_CAPABILITY")
    );
    ensure!(!fixture.cwd.join("blocked.bin").exists());
    println!(
        "PASS real MCP fileParams metadata/schema and fail-closed unconfigured attachment origin without signed URL disclosure"
    );
    let shell = created["terminal_job_id"]
        .as_str()
        .context("automatic terminal missing")?
        .to_owned();
    ensure!(created["active_job_id"] == shell);
    ensure!(call(&client, "run_command", json!({"session_id":sid})).await?["job_id"] == shell);
    call(
        &client,
        "send_stdin",
        json!({"session_id":sid,"text":"printf 'AUTO_%s\\n' 'SHELL_OK'\n"}),
    )
    .await?;
    until(&client, json!({"session_id":sid}), |r| {
        r["output"].as_str().unwrap().contains("AUTO_SHELL_OK")
    })
    .await?;
    ensure!(
        call(&client, "connect_session", json!({"session_id":sid})).await?["terminal_job_id"]
            == shell
    );
    call(
        &client,
        "resize_command",
        json!({"session_id":sid,"rows":75,"cols":199}),
    )
    .await?;
    call(
        &client,
        "send_stdin",
        json!({"session_id":sid,"text":"stty size\n"}),
    )
    .await?;
    until(&client, json!({"session_id":sid}), |r| {
        r["output"].as_str().unwrap().contains("75 199")
    })
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
    println!("PASS stdio session/files and real sandbox command/file read-write-edit");

    let approval_job = created["jobs"]
        .as_array()
        .context("missing jobs")?
        .iter()
        .find(|job| job["kind"] == "approvals")
        .context("missing approval terminal")?;
    ensure!(
        raw(
            &client,
            "send_stdin",
            json!({"session_id":sid,"job_id":approval_job["job_id"],"text":"y\n"})
        )
        .await?
        .is_error
            == Some(true)
    );
    let mut console = Command::new(&fixture.binary)
        .args(["approval-console", sid])
        .env_clear()
        .envs(&fixture.env)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()?;
    let mut console_input = console.stdin.take().context("missing console stdin")?;
    let mut console_output = console.stdout.take().context("missing console stdout")?;
    let (denied, human) = tokio::join!(
        raw(
            &client,
            "without_sandbox",
            json!({"session_id":sid,"command":["/bin/sh","-c","printf MUST_NOT_RUN > denied-marker"]})
        ),
        async {
            tokio::time::timeout(Duration::from_secs(5), async {
                let mut output = Vec::new();
                let mut chunk = [0; 1024];
                while !String::from_utf8_lossy(&output).contains("Allow without sandbox? [y/N]") {
                    let count = console_output.read(&mut chunk).await?;
                    ensure!(
                        count > 0 && output.len() < 65536,
                        "approval console closed before prompt"
                    );
                    output.extend_from_slice(&chunk[..count]);
                }
                console_input.write_all(b"n\n").await?;
                Ok::<_, anyhow::Error>(())
            })
            .await
            .context("approval prompt timed out")?
        }
    );
    human?;
    let denied = denied?;
    ensure!(denied.is_error == Some(true) && result_text(&denied)?.contains("user denied"));
    ensure!(!fixture.cwd.join("denied-marker").exists());
    drop(console_input);
    ensure!(
        tokio::time::timeout(Duration::from_secs(2), console.wait())
            .await??
            .success()
    );
    println!(
        "PASS local human approval console receives real without_sandbox request; denial prevents execution; MCP stdin rejects approval terminal; console detach preserves session"
    );

    let other_sid = "stdio-other";
    let other_cwd = fixture.cwd.join("other-project");
    tokio::fs::create_dir_all(&other_cwd).await?;
    call(
        &client,
        "create_session",
        json!({"session_id":other_sid,"cwd":other_cwd}),
    )
    .await?;
    let other = call(&client, "run_command", json!({"session_id":other_sid,"command":["/bin/bash","-c","printf SECOND_READY; read value; printf 'SECOND:%s\n' \"$value\"; sleep 30"]})).await?;
    until(&client, json!({"session_id":other_sid}), |r| {
        r["output"].as_str().unwrap().contains("SECOND_READY")
    })
    .await?;
    let parallel = call(&client, "run_command", json!({"session_id":sid,"command":["/bin/bash","-c","printf PARALLEL_READY; read value; printf 'PARALLEL:%s\n' \"$value\"; sleep 30"]})).await?;
    let parallel_args = json!({"session_id":sid,"job_id":parallel["job_id"]});
    until(&client, parallel_args.clone(), |r| {
        r["output"].as_str().unwrap().contains("PARALLEL_READY")
    })
    .await?;
    let job = call(
        &client,
        "run_command",
        json!({"session_id":sid,"command":["/bin/bash","-c","printf STDIO_READY; read value; printf 'STDIN:%s\n' \"$value\"; sleep 30"]}),
    )
    .await?;
    let args = json!({"session_id":sid});
    until(&client, args.clone(), |r| {
        r["output"].as_str().unwrap().contains("STDIO_READY")
    })
    .await?;
    ensure!(call(&client, "read_output", args.clone()).await?["job_id"] == job["job_id"]);
    ensure!(
        raw(
            &client,
            "read_output",
            json!({"session_id":sid,"job_id":other["job_id"]})
        )
        .await?
        .is_error
            == Some(true)
    );
    ensure!(
        raw(
            &client,
            "send_stdin",
            json!({"session_id":other_sid,"job_id":job["job_id"],"text":"WRONG","keys":["Enter"]})
        )
        .await?
        .is_error
            == Some(true)
    );
    call(
        &client,
        "send_stdin",
        json!({"session_id":other_sid,"text":"other-only","keys":["Enter"]}),
    )
    .await?;
    let second_output = until(&client, json!({"session_id":other_sid}), |r| {
        r["output"].as_str().unwrap().contains("SECOND:other-only")
    })
    .await?;
    ensure!(!second_output["output"].as_str().unwrap().contains("WRONG"));
    call(&client, "send_stdin", json!({"session_id":sid,"job_id":parallel["job_id"],"text":"parallel-only","keys":["Enter"]})).await?;
    until(&client, parallel_args.clone(), |r| {
        r["output"]
            .as_str()
            .unwrap()
            .contains("PARALLEL:parallel-only")
    })
    .await?;
    ensure!(call(&client, "read_output", args.clone()).await?["job_id"] == job["job_id"]);
    println!(
        "PASS automatic session shell/reuse, session-only stdin/output, separate sessions and multiple in-flight commands without cross-talk"
    );
    let finished_sid = "finished-detached";
    call(
        &client,
        "create_session",
        json!({"session_id":finished_sid,"cwd":fixture.cwd}),
    )
    .await?;
    let finished = call(&client, "run_command", json!({"session_id":finished_sid,"command":["/bin/bash","-c","printf run >> completed-counter; printf DETACHED_READY; sleep .3; printf DETACHED_FINAL; exit 9"]})).await?;
    ensure!(
        until(&client, json!({"session_id":finished_sid}), |r| r["output"]
            .as_str()
            .unwrap()
            .contains("DETACHED_READY"))
        .await?["status"]
            == "running"
    );
    client.cancel().await?;
    StdioFixture::wait_exit(first_pid).await?;
    tokio::time::sleep(Duration::from_millis(400)).await;

    let (client, second_pid) = fixture.connect().await?;
    ensure!(second_pid != first_pid, "stdio child was not replaced");
    let sessions = call(&client, "list_sessions", json!({})).await?;
    ensure!(sessions["sessions"].as_array().unwrap().len() == 3);
    call(
        &client,
        "connect_session",
        json!({"session_id":finished_sid}),
    )
    .await?;
    let completed = call(&client, "read_output", json!({"session_id":finished_sid})).await?;
    ensure!(
        completed["job_id"] == finished["job_id"]
            && completed["status"] == "completed"
            && completed["exit_code"] == 9
            && completed["output"]
                .as_str()
                .unwrap()
                .contains("DETACHED_FINAL")
    );
    ensure!(tokio::fs::read_to_string(fixture.cwd.join("completed-counter")).await? == "run");
    call(&client, "close_session", json!({"session_id":finished_sid})).await?;
    println!(
        "PASS private PTY holder survives real frontend exit; detached completion retains final output/exit 9 and reconnect never reexecutes command; real resize verified"
    );
    let info = call(&client, "connect_session", json!({"session_id":sid})).await?;
    ensure!(
        info["jobs"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v["job_id"] == job["job_id"])
    );
    ensure!(info["active_job_id"] == job["job_id"] && info["terminal_job_id"] == shell);
    call(
        &client,
        "send_stdin",
        json!({"session_id":sid,"text":"hello","keys":["Enter"]}),
    )
    .await?;
    ensure!(
        until(&client, args.clone(), |r| {
            r["output"].as_str().unwrap().contains("STDIN:hello")
        })
        .await?["status"]
            == "running"
    );
    call(&client, "stop_command", args.clone()).await?;
    ensure!(call(&client, "read_output", args).await?["status"] == "stopped");
    ensure!(call(&client, "read_output", parallel_args.clone()).await?["status"] == "running");
    call(&client, "stop_command", parallel_args).await?;
    ensure!(
        call(
            &client,
            "read_output",
            json!({"session_id":sid,"job_id":shell})
        )
        .await?["status"]
            == "running"
    );
    ensure!(
        call(&client, "read_output", json!({"session_id":other_sid})).await?["status"] == "running"
    );
    println!(
        "PASS Rust stdio actual exit/replacement, session/PTY broker reconnect, live stdin and stop"
    );

    let job = call(
        &client,
        "run_command",
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
        "run_command",
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
    let legacy_note = fixture
        .home
        .path()
        .join("state/sessions")
        .join(sid)
        .join("memo.md");
    tokio::fs::write(&legacy_note, "existing user note\n").await?;
    call(&client, "close_session", json!({"session_id":sid})).await?;
    ensure!(tokio::fs::read_to_string(&legacy_note).await? == "existing user note\n");
    ensure!(
        raw(&client, "read_output", json!({"session_id":sid}))
            .await?
            .is_error
            == Some(true)
    );
    ensure!(
        raw(&client, "run_command", json!({"session_id":sid}))
            .await?
            .is_error
            == Some(true)
    );
    ensure!(
        call(&client, "read_output", json!({"session_id":other_sid})).await?["status"] == "running"
    );
    let recreated = call(
        &client,
        "create_session",
        json!({"session_id":sid,"cwd":fixture.cwd}),
    )
    .await?;
    ensure!(recreated["terminal_job_id"] != shell);
    ensure!(
        raw(
            &client,
            "read_output",
            json!({"session_id":sid,"job_id":job["job_id"]})
        )
        .await?
        .is_error
            == Some(true)
    );
    call(&client, "close_session", json!({"session_id":sid})).await?;
    call(&client, "close_session", json!({"session_id":other_sid})).await?;
    ensure!(
        call(&client, "list_sessions", json!({})).await?["sessions"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    println!(
        "PASS individual command stop preserves other work; session close isolates other sessions; closed-session calls and stale job IDs rejected after recreate"
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
    let launcher = include_str!("../../deploy/dev-session-mcp-stdio")
        .replace("/home/devmcp/projects", &shell_path(&fixture.cwd))
        .replace("/home/devmcp", &shell_path(fixture.home.path()))
        .replace(
            "/opt/dev-session-mcp/bin/dev-session-mcp",
            &shell_path(&fixture.binary),
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
