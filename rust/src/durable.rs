//! Bounded recovery evidence, not a process supervisor. Never respawn from this store.
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, VecDeque},
    io::Write,
    path::PathBuf,
};

const RECORDS: usize = 128;
const CHECKPOINTS: usize = 128;
const HISTORY_BYTES: usize = 1024 * 1024;
const STORE_BYTES: usize = 4 * 1024 * 1024;

#[derive(Clone, Default, Serialize, Deserialize)]
struct Data {
    version: u32,
    records: BTreeMap<String, Value>,
    intents: BTreeMap<String, Value>,
    checkpoints: BTreeMap<String, Value>,
    events: VecDeque<Value>,
}
pub struct Store {
    path: PathBuf,
    data: Data,
    // Once a write is uncertain, no more mutations or starts in this broker lifetime.
    fault: bool,
    event_bytes: usize,
    dirty: bool,
    durable_positions: BTreeMap<String, Value>,
}
impl Store {
    pub fn open(path: PathBuf) -> Result<Self> {
        // A missing snapshot in an initialized store is lost evidence, not a fresh keyspace.
        let marker = path.with_extension("initialized");
        ensure!(
            path.exists() || !marker.exists(),
            "recovery snapshot lost; refusing a fresh idempotency keyspace"
        );
        let data = if path.exists() {
            ensure!(
                std::fs::metadata(&path)?.len() <= STORE_BYTES as u64,
                "recovery store oversized"
            );
            let data: Data = serde_json::from_slice(&std::fs::read(&path)?)?;
            ensure!(data.version == 1, "unsupported recovery store version");
            data
        } else {
            Data {
                version: 1,
                ..Data::default()
            }
        };
        ensure!(
            data.records.len() <= RECORDS && data.checkpoints.len() <= CHECKPOINTS,
            "invalid recovery store bounds"
        );
        for (id, record) in &data.records {
            ensure!(
                record["execution_id"] == id.as_str()
                    && record["session_id"].is_string()
                    && record["work_id"].is_string()
                    && record["initial_cursor"]
                        .as_str()
                        .is_some_and(|v| cursor(v).is_ok())
                    && record["cursor"].as_str().is_some_and(|v| cursor(v).is_ok())
                    && matches!(
                        record["status"].as_str(),
                        Some("starting" | "running" | "exited" | "outcome_unknown")
                    ),
                "invalid execution evidence"
            );
        }
        for intent in data.intents.values() {
            ensure!(
                intent["execution_id"]
                    .as_str()
                    .is_some_and(|id| data.records.contains_key(id))
                    && intent["fingerprint"].is_string(),
                "invalid start intent"
            );
        }
        ensure!(
            data.events.len() <= 1024 && data.intents.len() <= RECORDS,
            "invalid recovery history bounds"
        );
        for event in &data.events {
            ensure!(
                event["cursor"].as_str().is_some_and(|v| cursor(v).is_ok())
                    && event["execution_id"]
                        .as_str()
                        .is_some_and(|id| data.records.contains_key(id))
                    && event["sequence"].is_u64(),
                "invalid recovery event"
            );
        }
        for checkpoint in data.checkpoints.values() {
            ensure!(
                checkpoint["revision"].as_u64().is_some_and(|v| v > 0)
                    && checkpoint["detail_cursor"]
                        .as_str()
                        .is_some_and(|v| cursor(v).is_ok())
                    && checkpoint["execution_id"]
                        .as_str()
                        .is_some_and(|id| data.records.contains_key(id)),
                "invalid reader checkpoint"
            );
        }
        // Marker first: a crash during initial snapshot creation fails closed on restart.
        if !marker.exists() {
            let mut file = std::fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&marker)?;
            file.write_all(b"1\n")?;
            file.sync_all()?;
            std::fs::File::open(path.parent().context("recovery path has no parent")?)?
                .sync_all()?;
        }
        let event_bytes = data
            .events
            .iter()
            .map(|v| serde_json::to_vec(v).unwrap().len())
            .sum();
        ensure!(
            event_bytes <= HISTORY_BYTES,
            "invalid recovery history size"
        );
        let mut store = Self {
            path,
            data,
            fault: false,
            event_bytes,
            dirty: false,
            durable_positions: BTreeMap::new(),
        };
        for record in store.data.records.values_mut() {
            if matches!(record["status"].as_str(), Some("running" | "starting")) {
                record["status"] = json!("outcome_unknown");
                record["history_tail_unknown"] = json!(true);
                record["recovery_reason"] = json!("broker_restarted_without_terminal_evidence");
            }
        }
        store.save()?;
        Ok(store)
    }
    fn save(&mut self) -> Result<()> {
        ensure!(
            !self.fault,
            "recovery persistence unavailable; retain IDs and do not resend commands or input"
        );
        let bytes = serde_json::to_vec(&self.data)?;
        ensure!(
            bytes.len() <= STORE_BYTES,
            "recovery store capacity reached; no automatic eviction of execution or idempotency records"
        );
        let result = (|| -> Result<()> {
            let temporary = self.path.with_extension("tmp");
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .truncate(true)
                .write(true)
                .open(&temporary)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            std::fs::rename(temporary, &self.path)?;
            std::fs::File::open(self.path.parent().context("recovery path has no parent")?)?
                .sync_all()?;
            Ok(())
        })();
        if result.is_err() {
            self.fault = true;
        } else {
            self.dirty = false;
            self.durable_positions = self
                .data
                .records
                .iter()
                .map(|(id, v)| (id.clone(), v["cursor"].clone()))
                .collect();
        }
        result.context("recovery write uncertain; do not resend commands or input")
    }
    pub fn healthy(&self) -> bool {
        !self.fault
    }
    pub fn records(&self) -> Vec<Value> {
        self.data.records.values().map(brief).collect()
    }
    pub fn checkpoints(&self) -> Vec<Value> {
        self.data.checkpoints.values().cloned().collect()
    }
    pub fn record(&self, id: &str) -> Option<&Value> {
        self.data.records.get(id)
    }
    pub fn replay(&self, key: &str, fingerprint: &str) -> Result<Option<Value>> {
        ensure!(
            !self.fault,
            "recovery persistence unavailable; do not resend"
        );
        if let Some(intent) = self.data.intents.get(key) {
            ensure!(
                intent["fingerprint"] == fingerprint,
                "idempotency key reused with different request"
            );
            let mut record = brief(
                &self.data.records[intent["execution_id"].as_str().context("invalid intent")?],
            );
            record["replayed"] = json!(true);
            return Ok(Some(record));
        }
        Ok(None)
    }
    pub fn reserve(
        &mut self,
        record: Value,
        key: Option<String>,
        fingerprint: String,
    ) -> Result<()> {
        ensure!(
            !self.fault,
            "recovery persistence unavailable; no execution started"
        );
        ensure!(
            self.data.records.len() < RECORDS,
            "128 durable executions reached; no execution started and no idempotency keys evicted"
        );
        let before = self.data.clone();
        let id = record["execution_id"]
            .as_str()
            .context("missing execution ID")?
            .to_owned();
        self.data.records.insert(id.clone(), record);
        if let Some(key) = key {
            self.data
                .intents
                .insert(key, json!({"fingerprint":fingerprint,"execution_id":id}));
        }
        if let Err(error) = self.save() {
            self.data = before;
            return Err(error);
        }
        Ok(())
    }
    pub fn append(&mut self, mut record: Value, mut event: Value) {
        let id = record["execution_id"].as_str().unwrap().to_owned();
        event["cursor"] = record["cursor"].clone();
        record["command"] = self
            .data
            .records
            .get(&id)
            .map(|v| v["command"].clone())
            .unwrap_or(Value::Null);
        self.data.records.insert(id, record);
        let output = event["kind"] == "output";
        self.event_bytes += serde_json::to_vec(&event).unwrap().len();
        self.data.events.push_back(event);
        while self.event_bytes > HISTORY_BYTES || self.data.events.len() > 1024 {
            self.event_bytes -= serde_json::to_vec(&self.data.events.pop_front().unwrap())
                .unwrap()
                .len();
        }
        self.dirty = true;
        // Output is batched; intent, lifecycle, input receipts and checkpoints commit synchronously.
        if !output && self.flush().is_err() {
            self.fault = true;
        }
    }
    pub fn flush(&mut self) -> Result<()> {
        if self.dirty {
            if let Err(error) = self.save() {
                self.fault = true;
                return Err(error);
            }
        }
        Ok(())
    }
    pub fn durable_cursor(&self, id: &str) -> Value {
        self.durable_positions
            .get(id)
            .cloned()
            .unwrap_or(Value::Null)
    }

    pub fn read(&self, args: &Value) -> Result<Value> {
        let id = crate::workspace::text(args, "execution_id")?;
        let record = self
            .record(id)
            .context("execution unavailable; outcome unknown, never restart automatically")?;
        let (epoch, _) = cursor(
            record["cursor"]
                .as_str()
                .context("missing recovery cursor")?,
        )?;
        let end = self
            .data
            .records
            .values()
            .filter_map(|v| cursor(v["cursor"].as_str()?).ok())
            .filter(|(e, _)| *e == epoch)
            .map(|(_, n)| n)
            .max()
            .unwrap_or(0);
        let earliest = self
            .data
            .events
            .iter()
            .filter_map(|v| {
                let (e, n) = cursor(v["cursor"].as_str()?).ok()?;
                (e == epoch).then_some(n)
            })
            .min()
            .unwrap_or(end + 1);
        let supplied = args.get("cursor").and_then(Value::as_str);
        let (position, gap) = if let Some(value) = supplied {
            let (e, n) = cursor(value)?;
            let gap = e != epoch || n > end || n.saturating_add(1) < earliest;
            (if gap { earliest.saturating_sub(1) } else { n }, gap)
        } else {
            (end, false)
        };
        let mut events = Vec::new();
        let mut bytes = 0;
        let mut next = position;
        let mut more = false;
        for event in &self.data.events {
            let (e, n) = cursor(event["cursor"].as_str().unwrap())?;
            if e != epoch || n <= position || n > end {
                continue;
            }
            if event["execution_id"] == id {
                let size = serde_json::to_vec(event)?.len();
                if bytes + size > 32 * 1024 {
                    more = true;
                    break;
                }
                let mut event = event.clone();
                event.as_object_mut().unwrap().remove("cursor");
                events.push(event);
                bytes += size;
            }
            next = n;
        }
        if !more {
            next = end;
        }
        Ok(
            json!({"execution":brief(record),"events":events,"cursor":format!("{epoch}:{next}"),
            "earliest_cursor":format!("{epoch}:{}",earliest.saturating_sub(1)),"catch_up_required":gap,"more":more,
            "history_lost":gap,"persistence_ok":self.healthy(),"durable_cursor":self.durable_cursor(id),"observed_cursor":format!("{epoch}:{end}")}),
        )
    }
    pub fn source(&self, id: &str) -> Result<Value> {
        let record = self.record(id).context("execution unavailable")?;
        Ok(json!({"execution_id":id,"command":record["command"]}))
    }
    pub fn checkpoint(&mut self, args: &Value) -> Result<Value> {
        ensure!(
            args.as_object()
                .context("expected object")?
                .keys()
                .all(|k| matches!(
                    k.as_str(),
                    "op" | "execution_id"
                        | "reader_id"
                        | "expected_revision"
                        | "cursor"
                        | "purpose"
                        | "completion_condition"
                )),
            "unknown checkpoint argument"
        );
        let id = crate::workspace::text(args, "execution_id")?;
        let reader = short(args, "reader_id", 128)?;
        let position = short(args, "cursor", 128)?;
        let revision = args["expected_revision"]
            .as_u64()
            .context("expected_revision required")?;
        let record = self.record(id).context("execution unavailable")?;
        let key = digest(&json!([record["session_id"], record["work_id"], reader]))?;
        let previous = self.data.checkpoints.get(&key);
        ensure!(
            previous.map_or(0, |v| v["revision"].as_u64().unwrap()) == revision,
            "checkpoint revision conflict; read current checkpoint before retry"
        );
        ensure!(
            previous.is_some() || self.data.checkpoints.len() < CHECKPOINTS,
            "checkpoint capacity reached"
        );
        let (epoch, sequence) = cursor(&position)?;
        let (record_epoch, _) =
            cursor(record["cursor"].as_str().context("invalid record cursor")?)?;
        let record_sequence = self
            .data
            .records
            .values()
            .filter_map(|v| cursor(v["cursor"].as_str()?).ok())
            .filter(|(e, _)| *e == record_epoch)
            .map(|(_, n)| n)
            .max()
            .unwrap_or(0);
        ensure!(
            epoch == record_epoch && sequence <= record_sequence,
            "checkpoint cursor does not belong to execution history"
        );
        if let Some(old) = previous.filter(|v| v["execution_id"] == id) {
            let (old_epoch, old_sequence) = cursor(old["detail_cursor"].as_str().unwrap())?;
            ensure!(
                epoch == old_epoch && sequence >= old_sequence,
                "checkpoint cannot move backwards"
            );
        }
        let purpose = if args.get("purpose").is_some() {
            short(args, "purpose", 2048)?
        } else {
            previous.unwrap_or(record)["purpose"]
                .as_str()
                .unwrap_or("")
                .to_owned()
        };
        let condition = if args.get("completion_condition").is_some() {
            short(args, "completion_condition", 2048)?
        } else {
            previous.unwrap_or(record)["completion_condition"]
                .as_str()
                .unwrap_or("")
                .to_owned()
        };
        ensure!(
            !purpose.is_empty() && !condition.is_empty(),
            "purpose and completion_condition required for checkpoint"
        );
        let value = json!({"execution_id":id,"work_id":record["work_id"],"session_id":record["session_id"],
            "reader_id":reader,"detail_cursor":position,"revision":revision.checked_add(1).context("revision exhausted")?,
            "purpose":purpose,"completion_condition":condition});
        let before = self.data.clone();
        self.data.checkpoints.insert(key, value.clone());
        if let Err(error) = self.save() {
            self.data = before;
            return Err(error);
        }
        Ok(value)
    }
}
pub fn short(args: &Value, key: &str, limit: usize) -> Result<String> {
    let s = crate::workspace::text(args, key)?;
    ensure!(!s.is_empty() && s.len() <= limit, "invalid {key} length");
    Ok(s.to_owned())
}
pub fn digest(value: &Value) -> Result<String> {
    Ok(format!("{:x}", Sha256::digest(serde_json::to_vec(value)?)))
}
pub fn cursor(value: &str) -> Result<(&str, u64)> {
    let (epoch, seq) = value.rsplit_once(':').context("invalid cursor")?;
    ensure!(!epoch.is_empty(), "invalid cursor epoch");
    Ok((epoch, seq.parse()?))
}
/// Observation never acknowledges detail. Only an explicit checkpoint CAS does that.
pub fn project(mut data: Value, args: &Value) -> Result<Value> {
    let view = args
        .get("view")
        .map(|v| v.as_str().context("invalid view"))
        .transpose()?
        .unwrap_or("detail");
    ensure!(matches!(view, "summary" | "detail"), "invalid view");
    if view == "detail" {
        return Ok(data);
    }
    let detail = args
        .get("cursor")
        .cloned()
        .unwrap_or_else(|| data["execution"]["initial_cursor"].clone());
    data["state_cursor"] = data
        .get("observed_cursor")
        .cloned()
        .unwrap_or_else(|| data["execution"]["cursor"].clone());
    data["detail_cursor"] = detail.clone();
    data["cursor"] = detail.clone();
    data["details_uri"] = json!(format!(
        "dev-session:///executions/{}?cursor={}",
        data["execution"]["execution_id"].as_str().unwrap_or(""),
        detail.as_str().unwrap_or("")
    ));
    data["detail_available"] =
        json!(data["events"].as_array().is_some_and(|v| !v.is_empty()) || data["more"] == true);
    data.as_object_mut().unwrap().remove("events");
    // Source is available only through explicit detail/source reads.
    data["execution"]
        .as_object_mut()
        .map(|v| v.remove("command"));
    Ok(data)
}

fn brief(record: &Value) -> Value {
    let mut value = record.clone();
    value.as_object_mut().unwrap().remove("command");
    if value["status"] == "starting" {
        value["status"] = json!("outcome_unknown");
        value["recovery_reason"] = json!("start_not_confirmed");
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;
    fn record(id: &str, status: &str) -> Value {
        json!({"execution_id":id,"session_id":"session","work_id":id,"status":status,
            "cursor":"epoch:0","initial_cursor":"epoch:0","command":["SOURCE_CANARY"],
            "purpose":"test purpose","completion_condition":"verified fixture exit"})
    }
    #[test]
    fn missing_or_corrupt_snapshot_never_becomes_a_fresh_store() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let path = temp.path().join("recovery.json");
        drop(Store::open(path.clone())?);
        std::fs::remove_file(&path)?;
        assert!(Store::open(path.clone()).is_err());
        std::fs::write(&path, b"{")?;
        assert!(Store::open(path).is_err());
        Ok(())
    }
    #[test]
    fn intent_crash_never_replays_and_conflicts_reject() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let path = temp.path().join("recovery.json");
        let mut store = Store::open(path.clone())?;
        store.reserve(
            record("execution", "starting"),
            Some("key".into()),
            "hash".into(),
        )?;
        drop(store);
        let store = Store::open(path)?;
        let replay = store.replay("key", "hash")?.unwrap();
        assert_eq!(replay["execution_id"], "execution");
        assert_eq!(replay["status"], "outcome_unknown");
        assert!(!replay.to_string().contains("SOURCE_CANARY"));
        assert!(store.replay("key", "different").is_err());
        Ok(())
    }
    #[test]
    fn bounded_store_preserves_keys_and_rejects_extra_records() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut store = Store::open(temp.path().join("recovery.json"))?;
        for n in 0..RECORDS {
            store.reserve(
                record(&n.to_string(), "exited"),
                Some(n.to_string()),
                "hash".into(),
            )?;
        }
        assert!(
            store
                .reserve(record("overflow", "starting"), None, "hash".into())
                .is_err()
        );
        assert!(store.replay("0", "hash")?.is_some());
        assert_eq!(store.records().len(), RECORDS);
        assert!(std::fs::metadata(&store.path)?.len() <= STORE_BYTES as u64);
        Ok(())
    }
    #[test]
    fn reader_cas_and_failed_write_do_not_acknowledge() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut store = Store::open(temp.path().join("recovery.json"))?;
        store.reserve(record("execution", "exited"), None, "hash".into())?;
        let mut args = json!({"execution_id":"execution","reader_id":"one","cursor":"epoch:0","expected_revision":0});
        assert_eq!(store.checkpoint(&args)?["revision"], 1);
        assert!(store.checkpoint(&args).is_err());
        args["reader_id"] = json!("two");
        assert_eq!(store.checkpoint(&args)?["revision"], 1);
        let old = store.checkpoints();
        std::fs::create_dir(store.path.with_extension("tmp"))?;
        args["expected_revision"] = json!(1);
        assert!(store.checkpoint(&args).is_err());
        assert_eq!(store.checkpoints(), old);
        assert!(!store.healthy());
        assert!(
            store
                .reserve(record("new", "starting"), None, "hash".into())
                .is_err()
        );
        let restarted = Store::open(store.path.clone());
        assert!(restarted.is_err()); // Fail closed; never replace an unreadable store.
        Ok(())
    }
    #[test]
    fn summary_and_restarted_history_preserve_detail_position_and_report_loss() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let path = temp.path().join("recovery.json");
        let mut store = Store::open(path.clone())?;
        let mut rec = record("execution", "running");
        store.reserve(rec.clone(), None, "hash".into())?;
        for n in 1..=80 {
            rec["cursor"] = json!(format!("epoch:{n}"));
            store.append(rec.clone(), json!({"execution_id":"execution","sequence":n,"kind":"output","data":{"text":"x".repeat(16000)}}));
        }
        assert!(store.healthy());
        let args = json!({"execution_id":"execution","cursor":"epoch:0","view":"summary"});
        let summary = project(store.read(&args)?, &args)?;
        assert_eq!(summary["detail_cursor"], "epoch:0");
        assert_eq!(summary["state_cursor"], "epoch:80");
        assert_eq!(summary["catch_up_required"], true);
        assert!(summary.get("events").is_none());
        assert!(summary.to_string().len() < 2048);
        assert_eq!(store.read(&args)?, store.read(&args)?);
        store.flush()?;
        let disk = std::fs::metadata(&path)?.len();
        assert!(disk < 2 * HISTORY_BYTES as u64);
        drop(store);
        let store = Store::open(path)?;
        assert_eq!(store.read(&args)?["execution"]["status"], "outcome_unknown");
        assert_eq!(store.read(&args)?["catch_up_required"], true);
        Ok(())
    }
}
