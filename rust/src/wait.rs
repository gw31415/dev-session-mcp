//! Ordinary bounded read-only tool, implemented against the retained broker protocol.
use crate::broker::Client;
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::time::Duration;
use tokio::sync::Semaphore;

// Leave broker slots available for execution control. No queued background waiters.
static WAITERS: Semaphore = Semaphore::const_new(8);
const SNAPSHOT_BUDGET: Duration = Duration::from_secs(1);

async fn snapshot(broker: &Client, args: &Value) -> Result<Value> {
    tokio::time::timeout(SNAPSHOT_BUDGET, broker.call(args.clone()))
        .await
        .context("wait_execution snapshot deadline exceeded; retain your previous cursor")?
}

fn ready(data: &Value) -> bool {
    data["catch_up_required"] == true
        || data["more"] == true
        || data["events"]
            .as_array()
            .is_some_and(|events| !events.is_empty())
        || data["execution"]["status"] != "running"
}

fn response(mut data: Value, timed_out: bool) -> Value {
    data["timed_out"] = json!(timed_out);
    data
}

pub async fn execution(broker: &Client, args: Value) -> Result<Value> {
    // Validate view even on timeout; observer position is never a detail ACK.
    let view = args
        .get("view")
        .map(|v| v.as_str().context("invalid view"))
        .transpose()?
        .unwrap_or("detail");
    ensure!(matches!(view, "summary" | "detail"), "invalid view");
    ensure!(
        view == "summary" || args.get("state_cursor").is_none(),
        "state_cursor requires summary view"
    );
    for key in ["execution_id", "cursor"] {
        crate::durable::short(&args, key, 128)?;
    }
    let mut observing = args.clone();
    if let Some(cursor) = args.get("state_cursor") {
        observing["cursor"] = cursor.clone();
    }
    let data = execution_detail(broker, observing).await?;
    if view == "detail" {
        return Ok(data);
    }
    let mut detail = snapshot(
        broker,
        &json!({"op":"read_execution","execution_id":args["execution_id"],"cursor":args["cursor"]}),
    )
    .await?;
    detail["timed_out"] = data["timed_out"].clone();
    crate::durable::project(detail, &args)
}

async fn execution_detail(broker: &Client, args: Value) -> Result<Value> {
    let object = args.as_object().context("expected arguments object")?;
    ensure!(
        object.keys().all(|key| matches!(
            key.as_str(),
            "execution_id" | "cursor" | "max_wait_ms" | "view" | "state_cursor"
        )),
        "unknown wait_execution argument"
    );
    for key in ["execution_id", "cursor"] {
        let value = args[key]
            .as_str()
            .context("execution_id and cursor are required strings")?;
        ensure!(
            !value.is_empty() && value.len() <= 128,
            "invalid execution_id or cursor length"
        );
    }
    let milliseconds = match args.get("max_wait_ms") {
        None => 1000,
        Some(value) => value
            .as_u64()
            .context("max_wait_ms must be an integer from 0 to 10000")?,
    };
    ensure!(
        milliseconds <= 10000,
        "max_wait_ms must be an integer from 0 to 10000"
    );
    let _slot = WAITERS
        .try_acquire()
        .context("wait_execution capacity reached; no wait was queued")?;
    let read = json!({"op":"read_execution", "execution_id":args["execution_id"], "cursor":args["cursor"]});
    let initial = snapshot(broker, &read).await?;
    if ready(&initial) {
        return Ok(response(initial, false));
    }
    if milliseconds == 0 {
        return Ok(response(initial, true));
    }
    // Always retain the original caller cursor. No intermediate page is consumed.
    // Broker registers Notify before reading, closing the snapshot/wait race.
    let wait = json!({"op":"wait_events", "session_id":initial["execution"]["session_id"],
        "execution_id":args["execution_id"], "cursor":args["cursor"]});
    match tokio::time::timeout(Duration::from_millis(milliseconds), broker.call(wait)).await {
        Ok(result) => Ok(response(result?, false)),
        Err(_) => {
            // Dropping the wait closes its socket and releases the old broker's slot.
            // A final read also catches an event racing the deadline.
            let final_data = snapshot(broker, &read).await?;
            let timed_out = !ready(&final_data);
            Ok(response(final_data, timed_out))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use tokio::{
        io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
        net::UnixListener,
    };

    // An event arriving after snapshot but before wait registration must be recovered
    // using the original cursor, not the latest execution/global position.
    #[tokio::test]
    async fn snapshot_wait_race_preserves_caller_cursor() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("broker.sock");
        let listener = UnixListener::bind(&path)?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        let peer = tokio::spawn(async move {
            for op in ["read_execution", "wait_events"] {
                let (stream, _) = listener.accept().await.unwrap();
                let mut reader = BufReader::new(stream);
                let mut line = String::new();
                reader.read_line(&mut line).await.unwrap();
                let request: Value = serde_json::from_str(&line).unwrap();
                assert_eq!(request["op"], op);
                assert_eq!(request["cursor"], "epoch:3");
                assert_eq!(request["execution_id"], "target");
                let mut data = json!({"execution":{"session_id":"session", "status":"running"},
                    "events":[], "cursor":"epoch:7", "earliest_cursor":"epoch:0",
                    "catch_up_required":false,"more":false});
                if op == "wait_events" {
                    assert_eq!(request["session_id"], "session");
                    data["events"] = json!([{"sequence":8,"kind":"output"}]);
                    data["cursor"] = json!("epoch:8");
                }
                reader
                    .get_mut()
                    .write_all(format!("{}\n", json!({"ok":true,"result":data})).as_bytes())
                    .await
                    .unwrap();
            }
        });
        let result = execution(
            &Client::at(path),
            json!({"execution_id":"target","cursor":"epoch:3"}),
        )
        .await?;
        assert_eq!(result["events"][0]["sequence"], 8);
        assert_eq!(result["timed_out"], false);
        peer.await?;
        Ok(())
    }

    #[tokio::test]
    async fn unresponsive_snapshot_is_bounded_and_does_not_invent_state() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("broker.sock");
        let listener = UnixListener::bind(&path)?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        let peer = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut reader = BufReader::new(stream);
            let mut line = String::new();
            reader.read_line(&mut line).await.unwrap();
            line.clear();
            assert_eq!(
                reader.read_line(&mut line).await.unwrap(),
                0,
                "deadline must close socket"
            );
        });
        let began = tokio::time::Instant::now();
        let result = execution(
            &Client::at(path),
            json!({"execution_id":"target","cursor":"epoch:3","max_wait_ms":0}),
        )
        .await;
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("snapshot deadline")
        );
        assert!(began.elapsed() < Duration::from_secs(2));
        tokio::time::timeout(Duration::from_secs(1), peer).await??;
        Ok(())
    }
}
