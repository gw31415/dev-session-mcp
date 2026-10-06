mod common;
use anyhow::{Result, ensure};
use common::{Fixture, call, now, raw, result_text, until};
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use serde_json::json;
use std::time::Duration;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rust_http_stdio_and_durable_sessions() -> Result<()> {
    let fixture = Fixture::new().await?;
    let mut server = fixture.start(json!(["fixture-owner"])).await?;
    fixture.ready(&mut server).await?;
    let response = fixture.http.get(&fixture.resource).send().await?;
    ensure!(
        response.status().as_u16() == 401,
        "unauthenticated HTTP accepted"
    );
    ensure!(
        response
            .headers()
            .get("www-authenticate")
            .unwrap()
            .to_str()?
            .contains("oauth-protected-resource/mcp"),
        "missing resource challenge"
    );
    let metadata: serde_json::Value = fixture
        .http
        .get(
            fixture
                .resource
                .replace("/mcp", "/.well-known/oauth-protected-resource/mcp"),
        )
        .send()
        .await?
        .json()
        .await?;
    ensure!(
        metadata["resource"] == fixture.resource
            && metadata["authorization_servers"] == json!([fixture.issuer()]),
        "wrong protected-resource metadata"
    );
    ensure!(
        fixture.authorize(true, false).await?.0 == 400
            && fixture.authorize(false, true).await?.0 == 400,
        "fixture accepted wrong verifier/resource"
    );
    let (status, grant) = fixture.authorize(false, false).await?;
    ensure!(status == 200, "fixture code exchange failed");
    let access = grant["access_token"].as_str().unwrap().to_owned();
    println!(
        "PASS Rust server discovery/401 + hand-written fixture PKCE/resource exchange (not SDK OAuth linking)"
    );
    for (label, claims, status) in [
        ("aud", json!({"aud":"https://wrong.invalid/mcp"}), 401),
        ("iss", json!({"iss":"https://wrong.invalid/"}), 401),
        ("exp", json!({"exp":now()-120}), 401),
        ("nbf", json!({"nbf":now()+120}), 401),
        ("owner", json!({"sub":"other"}), 403),
        ("scope", json!({"scope":"openid"}), 403),
        ("missing exp", json!({"exp":null}), 401),
        ("missing sub", json!({"sub":null}), 401),
    ] {
        let bearer = fixture.token(claims)?;
        ensure!(
            fixture
                .http
                .get(&fixture.resource)
                .bearer_auth(bearer)
                .send()
                .await?
                .status()
                .as_u16()
                == status,
            "JWT {label} accepted"
        );
    }
    let (prefix, signature) = access.rsplit_once('.').unwrap();
    let mut bytes = signature.as_bytes().to_vec();
    bytes[0] = if bytes[0] == b'A' { b'B' } else { b'A' };
    let wrong_signature = format!("{prefix}.{}", String::from_utf8(bytes)?);
    let none = "eyJhbGciOiJub25lIn0.eyJzdWIiOiJmaXh0dXJlLW93bmVyIn0.";
    let hs = jsonwebtoken::encode(
        &Header::new(Algorithm::HS256),
        &json!({"iss":fixture.issuer(),"aud":fixture.resource,"sub":"fixture-owner","scope":"mcp:tools","exp":now()+60}),
        &EncodingKey::from_secret(b"fixture-only-HS-key"),
    )?;
    for bearer in [wrong_signature.as_str(), none, &hs] {
        ensure!(
            fixture
                .http
                .get(&fixture.resource)
                .bearer_auth(bearer)
                .send()
                .await?
                .status()
                .as_u16()
                == 401,
            "bad signature/algorithm accepted"
        );
    }
    ensure!(
        fixture
            .http
            .get(&fixture.resource)
            .query(&[("access_token", &access)])
            .send()
            .await?
            .status()
            .as_u16()
            == 401,
        "query token accepted"
    );
    ensure!(
        fixture
            .http
            .get(&fixture.resource)
            .bearer_auth(&access)
            .header("Accept", "application/json, text/event-stream")
            .header("Origin", "https://evil.invalid")
            .send()
            .await?
            .status()
            .as_u16()
            == 403,
        "evil Origin accepted"
    );
    let host_response = fixture
        .http
        .get(&fixture.resource)
        .bearer_auth(&access)
        .header("Accept", "application/json, text/event-stream")
        .header("Host", "evil.invalid")
        .send()
        .await?;
    ensure!(
        host_response.status().as_u16() == 403
            && host_response
                .text()
                .await?
                .contains("Host header is not allowed"),
        "missing forbidden-Host response"
    );
    println!(
        "PASS Rust JWT issuer/audience/time/owner/scope/signature/algorithm/query and Origin/Host rejection"
    );
    let discovery = fixture
        .modern(&access, "server/discover", json!({}))
        .await?;
    ensure!(
        discovery["supportedVersions"]
            .as_array()
            .unwrap()
            .contains(&json!("2026-07-28")),
        "current protocol unavailable"
    );
    ensure!(
        fixture.modern(&access, "tools/list", json!({})).await?["tools"]
            .as_array()
            .unwrap()
            .len()
            == 20,
        "wrong current tool count"
    );
    let result = fixture
        .modern(
            &access,
            "tools/call",
            json!({"name":"list_sessions","arguments":{}}),
        )
        .await?;
    ensure!(
        result["resultType"] == "complete",
        "missing current result type"
    );
    let mut client = fixture.connect(&access).await?;
    ensure!(
        client.list_tools(None).await?.tools.len() == 20,
        "official Rust client tool count"
    );
    println!("PASS current stateless HTTP and official Rust SDK client: 20 tools");
    let sid = format!("rust.{}", uuid::Uuid::new_v4());
    let session = call(
        &client,
        "create_session",
        json!({"session_id":sid,"cwd":fixture.cwd}),
    )
    .await?;
    ensure!(
        session["cwd"] == fixture.cwd.display().to_string(),
        "wrong cwd"
    );
    ensure!(
        call(&client, "list_sessions", json!({})).await?["sessions"]
            .as_array()
            .unwrap()
            .len()
            == 1,
        "session missing"
    );
    call(
        &client,
        "set_memo",
        json!({"session_id":sid,"text":"Purpose: Rust suite\nNext: reconnect"}),
    )
    .await?;
    ensure!(
        call(&client, "get_memo", json!({"session_id":sid})).await?["text"]
            .as_str()
            .unwrap()
            .contains("reconnect"),
        "memo missing"
    );
    let result = raw(
        &client,
        "execute",
        json!({"session_id":sid,"command":["/bin/sh","-c","printf RUST_EXEC_OK"]}),
    )
    .await?;
    ensure!(
        result.is_error != Some(true) && result_text(&result)?.contains("RUST_EXEC_OK"),
        "sandbox execute failed"
    );
    for content in ["before\n", "after\n"] {
        let result = raw(
            &client,
            "write_file",
            json!({"session_id":sid,"path":"edit.txt","content":content}),
        )
        .await?;
        ensure!(result.is_error != Some(true), "sandbox write failed");
    }
    ensure!(
        result_text(
            &raw(
                &client,
                "read_file",
                json!({"session_id":sid,"path":"edit.txt"})
            )
            .await?
        )? == "after\n",
        "file edit failed"
    );
    let modern = fixture
        .modern(
            &access,
            "tools/call",
            json!({"name":"read_file","arguments":{"session_id":sid,"path":"edit.txt"}}),
        )
        .await?;
    ensure!(
        modern["resultType"] == "complete" && modern["content"][0]["text"] == "after\n",
        "current read failed"
    );
    println!("PASS Rust session/list/memo and embedded sandbox exec/read/write/edit");
    let upstream=call(&client,"start_command",json!({"session_id":sid,"command":["/bin/sh","-c","sleep 2; printf UPSTREAM_RUST_RECONNECT"]})).await?;
    let job=call(&client,"run_command",json!({"session_id":sid,"command":["/bin/bash","-c","read value; printf 'STDIN:%s\\n' \"$value\"; sleep 30"]})).await?;
    let job_args = json!({"session_id":sid,"job_id":job["job_id"]});
    client.cancel().await?;
    client = fixture.connect(&access).await?;
    let info = call(&client, "connect_session", json!({"session_id":sid})).await?;
    ensure!(
        info["jobs"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v["job_id"] == job["job_id"]),
        "durable job undiscoverable"
    );
    ensure!(
        info["upstream_jobs"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v["job_id"] == upstream["job_id"]),
        "upstream job undiscoverable"
    );
    call(
        &client,
        "send_stdin",
        json!({"session_id":sid,"job_id":job["job_id"],"text":"hello","keys":["Enter"]}),
    )
    .await?;
    ensure!(
        until(&client, job_args.clone(), |r| r["output"]
            .as_str()
            .unwrap()
            .contains("STDIN:hello"))
        .await?["status"]
            == "running",
        "stdin failed"
    );
    tokio::time::sleep(Duration::from_millis(2100)).await;
    ensure!(
        result_text(
            &raw(
                &client,
                "poll_job",
                json!({"session_id":sid,"job_id":upstream["job_id"]})
            )
            .await?
        )?
        .contains("UPSTREAM_RUST_RECONNECT"),
        "upstream reconnect failed"
    );
    println!(
        "PASS official Rust client disconnect/reconnect, job discovery, live stdin and ordinary job continuation"
    );
    call(&client, "stop_command", job_args.clone()).await?;
    ensure!(
        call(&client, "read_output", job_args).await?["status"] == "unavailable",
        "stop failed"
    );
    let output=call(&client,"run_command",json!({"session_id":sid,"command":["/bin/bash","-c","for i in {1..300}; do printf '%0100d\\n' \"$i\"; done; exit 7"]})).await?;
    let capped = until(
        &client,
        json!({"session_id":sid,"job_id":output["job_id"],"max_output_bytes":1024}),
        |r| r["status"] == "completed",
    )
    .await?;
    ensure!(
        capped["output_bytes"].as_u64().unwrap() <= 1024
            && capped["truncated"] == true
            && capped["exit_code"] == 7,
        "output limit/exit failed"
    );
    let result=raw(&client,"execute",json!({"session_id":sid,"command":["/bin/sh","-c","head -c 100000 /dev/zero | tr '\\0' x"]})).await?;
    let text = result_text(&result)?;
    ensure!(
        result.is_error != Some(true) && text.len() <= 65536 && text.contains("truncated"),
        "upstream response limit failed"
    );
    let long = call(
        &client,
        "start_command",
        json!({"session_id":sid,"command":["/bin/sh","-c","sleep 30"]}),
    )
    .await?;
    ensure!(
        raw(&client, "close_session", json!({"session_id":sid}))
            .await?
            .is_error
            == Some(true),
        "active-job close guard failed"
    );
    ensure!(
        raw(
            &client,
            "stop_job",
            json!({"session_id":sid,"job_id":long["job_id"]})
        )
        .await?
        .is_error
            != Some(true),
        "upstream stop failed"
    );
    println!("PASS Rust terminal/upstream output bounds, exit, stop and close guard");
    let durable=call(&client,"run_command",json!({"session_id":sid,"command":["/bin/bash","-c","printf RUST_SERVER_RESTART_OK; sleep 30"]})).await?;
    let durable_args = json!({"session_id":sid,"job_id":durable["job_id"]});
    until(&client, durable_args.clone(), |r| {
        r["output"]
            .as_str()
            .unwrap()
            .contains("RUST_SERVER_RESTART_OK")
    })
    .await?;
    client.cancel().await?;
    server.stop().await?;
    ensure!(!server.stderr()?.contains(&access), "bearer logged");
    server = fixture.start(json!(["fixture-owner"])).await?;
    fixture.ready(&mut server).await?;
    client = fixture.connect(&access).await?;
    ensure!(
        call(&client, "read_output", durable_args).await?["status"] == "running",
        "restart lost process"
    );
    ensure!(
        call(&client, "get_memo", json!({"session_id":sid})).await?["text"]
            .as_str()
            .unwrap()
            .contains("reconnect"),
        "restart lost memo"
    );
    println!("PASS actual Rust server restart retains tmux process and memo");
    let environment = call(
        &client,
        "run_command",
        json!({"session_id":sid,"command":["/usr/bin/env"]}),
    )
    .await?;
    let environment = until(
        &client,
        json!({"session_id":sid,"job_id":environment["job_id"]}),
        |r| r["status"] == "completed",
    )
    .await?;
    let output = environment["output"].as_str().unwrap();
    ensure!(
        !["TOKEN", "CREDENTIAL", "CONTROL_PLANE", "TUNNEL_", "MCP_"]
            .iter()
            .any(|s| output.contains(s)),
        "shell credential environment leaked"
    );
    call(&client, "close_session", json!({"session_id":sid})).await?;
    ensure!(
        call(&client, "list_sessions", json!({})).await?["sessions"]
            .as_array()
            .unwrap()
            .is_empty(),
        "close failed"
    );
    fixture.leaked_environment_rejected().await?;
    client.cancel().await?;
    server.stop().await?;
    ensure!(!server.stderr()?.contains(&access), "bearer logged");
    let stdio = fixture.stdio().await?;
    ensure!(
        stdio.list_tools(None).await?.tools.len() == 20,
        "Rust stdio failed"
    );
    stdio.cancel().await?;
    println!(
        "PASS clean environment, explicit close, credential rejection, no bearer logs and official Rust stdio: 20 tools"
    );
    Ok(())
}
