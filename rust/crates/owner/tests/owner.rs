use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use sonic_rs::{json, JsonContainerTrait, JsonValueTrait, Value};

const HOST: &str = "captain.transcript/1";
const CORE: &str = "cc-transcript.snapshot/1";

struct Owner {
    child: Child,
    input: ChildStdin,
    output: ChildStdout,
    next: u64,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

fn limits() -> Value {
    json!({"max_read_bytes":1024*1024,"max_source_read_bytes":1024*1024,"max_events":1000,"max_items":256,
        "max_output_bytes":1024*1024,"max_discovery_entries":1000,"max_sources":100})
}

fn context() -> Value {
    json!({"claimant":"owner-test","admission":"hook",
        "authority":{"kind":"user","effective_uid":unsafe { libc::geteuid() }.to_string()},
        "registry_generation":""})
}

fn user(id: &str) -> String {
    format!(
        "{{\"type\":\"user\",\"uuid\":\"{id}\",\"sessionId\":\"s\",\"timestamp\":\"2026-01-02T03:04:05Z\",\"message\":{{\"content\":\"hello {id}\"}}}}\n"
    )
}

impl Owner {
    fn start() -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_cc-transcript-owner"))
            .args(["--build", "test"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let input = child.stdin.take().unwrap();
        let output = child.stdout.take().unwrap();
        let mut owner = Self {
            child,
            input,
            output,
            next: 0,
        };
        owner.send(
            &json!({"protocol":1,"op":"hello","build":"test","snapshot_config":{
            "max_read_bytes_per_step":8192,"max_events_per_step":128,"max_entry_bytes":8192,
            "max_retained_bytes":32*1024*1024,"reserved_hook_accounted_bytes":4096,
            "max_leases":16,"reserved_hook_leases":1}}),
        );
        assert_eq!(
            owner.receive_raw(),
            br#"{"protocol":1,"op":"hello","build":"test"}"#
        );
        owner
    }

    fn send(&mut self, frame: &Value) {
        let body = frame.to_string();
        self.input
            .write_all(&(body.len() as u32).to_be_bytes())
            .unwrap();
        self.input.write_all(body.as_bytes()).unwrap();
        self.input.flush().unwrap();
    }

    fn receive_raw(&mut self) -> Vec<u8> {
        let mut length = [0u8; 4];
        self.output.read_exact(&mut length).unwrap();
        let mut body = vec![0u8; u32::from_be_bytes(length) as usize];
        self.output.read_exact(&mut body).unwrap();
        body
    }

    fn call_raw(&mut self, request: Value) -> Vec<u8> {
        self.next += 1;
        let frame = json!({"protocol":1,"op":"snapshot_request","id":self.next,
            "snapshot":{"schema":HOST,"request":request,"tool_registry":[]},
            "snapshot_context":context()});
        self.send(&frame);
        self.receive_raw()
    }

    fn call(&mut self, request: Value) -> Value {
        let frame: Value = sonic_rs::from_slice(&self.call_raw(request)).unwrap();
        assert_eq!(frame["op"].as_str(), Some("snapshot_result"));
        assert_eq!(frame["id"].as_u64(), Some(self.next));
        frame["snapshot"]["response"].clone()
    }

    fn acquire(&mut self, path: &Path) -> Value {
        let mut response = self.call(json!({"schema":CORE,"id":"acquire","operation":"acquire",
            "path":path.to_string_lossy().as_ref(),"classifier":{"id":"native","version":"1"},
            "deadline_unix_ms":now_ms()+30_000,"limits":limits()}));
        while response["status"].as_str() == Some("incomplete") {
            let cursor = response["cursor"].as_str().unwrap().to_owned();
            response = self
                .call(json!({"schema":CORE,"id":"resume","operation":"resume","cursor":cursor}));
        }
        assert_eq!(response["status"].as_str(), Some("ok"), "{response}");
        response["data"]["description"]["handle"].clone()
    }

    fn review_events(&mut self, handle: &Value, watermark: Value, context_events: u64) -> Value {
        let response = self.call(json!({"schema":HOST,"id":"events","operation":"review_events",
            "deadline_unix_ms":now_ms()+30_000,"limits":limits(),
            "view":{"handle":handle,"classifier":{"id":"native","version":"1"},"selectors":[],"attachments":[]},
            "watermark":watermark,"context_events":context_events}));
        assert_eq!(response["status"].as_str(), Some("ok"), "{response}");
        response["data"].clone()
    }
}

impl Drop for Owner {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn positions(page: &Value) -> Vec<u64> {
    page["records_json"]
        .as_array()
        .unwrap()
        .iter()
        .map(|record| {
            let record: Value = sonic_rs::from_str(record.as_str().unwrap()).unwrap();
            record["i"].as_u64().unwrap()
        })
        .collect()
}

#[test]
fn hook_view_classifies_a_native_transcript() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("session.jsonl");
    std::fs::write(&path, user("u0") + &user("u1")).unwrap();
    let mut owner = Owner::start();
    let handle = owner.acquire(&path);
    let response = owner.call(json!({"schema":HOST,"id":"view","operation":"prepare_hook_view",
        "deadline_unix_ms":now_ms()+30_000,"limits":limits(),
        "view":{"handle":&handle,"classifier":{"id":"native","version":"1"},"selectors":[],"attachments":[]},
        "cwd":null,"droid":false}));
    assert_eq!(response["status"].as_str(), Some("ok"), "{response}");
    assert_eq!(
        response["data"],
        json!({"kind":"classifier","classifier":{"id":"native","version":"1"}})
    );
}

#[test]
fn review_events_page_only_events_past_the_watermark() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("session.jsonl");
    std::fs::write(&path, user("u0") + &user("u1") + &user("u2")).unwrap();
    let mut owner = Owner::start();
    let handle = owner.acquire(&path);
    let first = owner.review_events(&handle, json!(null), 8);
    assert_eq!(positions(&first), [0, 1, 2]);
    assert_eq!(
        (
            first["event_start"].as_u64(),
            first["fresh_start"].as_u64(),
            first["caught_up"].as_bool(),
            first["reset"].as_bool()
        ),
        (Some(0), Some(0), Some(true), Some(false))
    );
    let watermark = first["watermark"].clone();
    let idle = owner.review_events(&handle, watermark.clone(), 0);
    assert!(positions(&idle).is_empty());
    assert_eq!(idle["fresh_start"].as_u64(), Some(3));
    assert_eq!(idle["watermark"], watermark);

    OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all((user("u3") + &user("u4")).as_bytes())
        .unwrap();
    let handle = owner.acquire(&path);
    let appended = owner.review_events(&handle, watermark, 1);
    assert_eq!(positions(&appended), [2, 3, 4]);
    assert_eq!(
        (
            appended["event_start"].as_u64(),
            appended["fresh_start"].as_u64(),
            appended["caught_up"].as_bool(),
            appended["reset"].as_bool()
        ),
        (Some(2), Some(3), Some(true), Some(false))
    );

    let foreign = owner.review_events(&handle, json!("v1:0:0:0:0:3:ffffffffffffffff"), 1);
    assert_eq!(positions(&foreign), [0, 1, 2, 3, 4]);
    assert_eq!(foreign["reset"].as_bool(), Some(true));
}

#[test]
fn unsupported_domain_operation_fails_with_pythonic_wire_bytes() {
    let mut owner = Owner::start();
    let raw = owner.call_raw(
        json!({"schema":HOST,"id":"review","operation":"prepare_review",
        "deadline_unix_ms":now_ms()+30_000,"limits":limits(),
        "policy":{"id":"captain-review","version":"1"}}),
    );
    let usage = [
        "activity_lifts",
        "append_parses",
        "bytes_decoded",
        "cache_hits",
        "cold_parses",
        "discovery_entries_examined",
        "events_parsed",
        "generations_invalidated",
        "generations_published",
        "inflight_joins",
        "nonincremental_lowering_calls",
        "nonincremental_lowering_source_bytes",
        "output_bytes",
        "prepared_cache_bytes_read",
        "prepared_cache_reads",
        "requests_cancelled",
        "requests_failed",
        "source_bytes_read",
        "source_opens",
        "transport_bytes",
    ]
    .map(|key| format!("\"{key}\":{}", u8::from(key == "requests_failed")))
    .join(",");
    assert_eq!(
        String::from_utf8(raw).unwrap(),
        format!(
            "{{\"protocol\":1,\"op\":\"snapshot_result\",\"id\":1,\"snapshot\":{{\"schema\":\"{HOST}\",\"response\":{{\"schema\":\"{CORE}\",\"id\":\"review\",\"status\":\"invalid_request\",\"complete\":false,\"data\":null,\"cursor\":null,\"reason\":\"unregistered Captain evidence policy\",\"usage\":{{{usage}}}}}}}}}"
        )
    );
}

#[test]
fn an_empty_stdin_exits_cleanly_before_the_handshake() {
    let status = Command::new(env!("CARGO_BIN_EXE_cc-transcript-owner"))
        .args(["--build", "test"])
        .stdin(Stdio::null())
        .status()
        .unwrap();
    assert!(status.success());
}
