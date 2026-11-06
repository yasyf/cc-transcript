use std::ops::Range;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use cc_transcript_core::activity::opens_turn;
use cc_transcript_core::discovery::is_subagent_path;
use cc_transcript_core::protocol::starts_with_structural_noise;
use cc_transcript_core::pystr;
use cc_transcript_core::snapshot::{
    Cancellation, ClassifierCallback, EntryChunk, NativeStore, RequestWaits, SnapshotError, Status,
    WorkLimits,
};
use cc_transcript_core::snapshot_codec::{self, MAX_PAGE_BYTES, MAX_RECORDS};
use cc_transcript_core::snapshot_input::native_input;
use cc_transcript_core::types::{Entry, UserEntry};
use sha2::{Digest, Sha256};
use sonic_rs::{json, JsonContainerTrait, JsonValueMutTrait, JsonValueTrait, Value};

use crate::log::{self, cursor_digest, lookup, Field};
use crate::pyjson::{self, CANONICAL};
use crate::review_events;
use crate::scope::{Failure, Scope};

pub const CORE_SCHEMA: &str = "cc-transcript.snapshot/1";
pub const HOST_SCHEMA: &str = "captain.transcript/1";
const MAX_REGISTRY_GENERATIONS: usize = 64;
const SLOW_REQUEST_MS: f64 = 100.0;
const REFUSAL_LOG_INTERVAL_MS: u64 = 1_000;
const ADMISSION_REFUSAL_FIELDS: [&str; 24] = [
    "refusals",
    "reason",
    "foreground",
    "cap_bytes",
    "additional_bytes",
    "total_bytes",
    "entry_bytes",
    "index_bytes",
    "pending_bytes",
    "classifier_bytes",
    "label_bytes",
    "projection_bytes",
    "discovery_bytes",
    "graph_bytes",
    "metadata_bytes",
    "owned_bytes",
    "transient_bytes",
    "live_generations",
    "leases",
    "waiters",
    "loads",
    "parked_loads",
    "parked_load_bytes",
    "cache_owners",
];
const LOGGED_USAGE: [&str; 16] = [
    "source_opens",
    "source_bytes_read",
    "bytes_decoded",
    "events_parsed",
    "cold_parses",
    "append_parses",
    "discovery_entries_examined",
    "prepared_cache_reads",
    "prepared_cache_bytes_read",
    "activity_lifts",
    "cache_hits",
    "inflight_joins",
    "generations_published",
    "generations_invalidated",
    "nonincremental_lowering_calls",
    "nonincremental_lowering_source_bytes",
];

static USAGE_KEYS: LazyLock<Vec<String>> = LazyLock::new(|| {
    let schema: Value = sonic_rs::from_str(include_str!(
        "../../../../cc_transcript/snapshot_schema/response.schema.json"
    ))
    .expect("response schema JSON");
    schema["$defs"]["Usage"]["properties"]
        .as_object()
        .expect("usage properties")
        .iter()
        .map(|(key, _)| key.to_owned())
        .collect()
});

#[derive(Clone, Copy)]
enum ExecutionPath {
    CoreRequest,
    OwnedProjection,
    OtherOperation,
}

impl ExecutionPath {
    fn as_str(self) -> &'static str {
        match self {
            Self::CoreRequest => "core_request",
            Self::OwnedProjection => "owned_projection",
            Self::OtherOperation => "other_operation",
        }
    }
}

type RegistryKey = (String, String, String);

pub struct Owner {
    pub store: NativeStore,
    registry_generations: Mutex<Vec<(RegistryKey, String)>>,
    started: Instant,
    last_refusal_log_ms: AtomicU64,
}

fn invalid(reason: impl Into<String>) -> SnapshotError {
    SnapshotError::new(Status::InvalidRequest, reason)
}

pub(crate) fn mismatch() -> SnapshotError {
    invalid("input does not match snapshot schema")
}

pub(crate) fn text<'a>(value: &'a Value, key: &str) -> Result<&'a str, SnapshotError> {
    value
        .get(key)
        .and_then(|field| field.as_str())
        .ok_or_else(mismatch)
}

fn parsed(text: &str) -> Value {
    sonic_rs::from_str(text).expect("native reply JSON")
}

fn count(value: &Value, key: &str) -> Result<usize, SnapshotError> {
    value
        .get(key)
        .and_then(|field| field.as_u64())
        .and_then(|field| usize::try_from(field).ok())
        .ok_or_else(mismatch)
}

pub fn scope_limits(limits: &Value, deadline: &Value) -> Result<WorkLimits, SnapshotError> {
    let limits = native_input(limits)?;
    Ok(WorkLimits {
        max_read_bytes: count(&limits, "max_read_bytes")?,
        max_source_read_bytes: count(&limits, "max_source_read_bytes")?,
        max_events: count(&limits, "max_events")?,
        max_items: count(&limits, "max_items")?,
        max_output_bytes: count(&limits, "max_output_bytes")?,
        max_discovery_entries: count(&limits, "max_discovery_entries")?,
        max_sources: count(&limits, "max_sources")?,
        deadline_unix_ms: native_input(deadline)?.as_u64().ok_or_else(mismatch)?,
    })
}

fn user_events(
    chunks: &[Arc<EntryChunk>],
    range: Range<usize>,
    predicate: fn(&UserEntry) -> bool,
) -> Vec<bool> {
    range
        .map(|position| {
            let chunk = &chunks[chunks.partition_point(|chunk| chunk.start <= position) - 1];
            matches!(&chunk.entries[position - chunk.start], Entry::User(user) if predicate(user))
        })
        .collect()
}

fn lane_prompt(user: &UserEntry) -> bool {
    !(user.meta.is_meta || user.meta.is_compact_summary || user.interrupted())
        && !pystr::strip(&user.content.text()).is_empty()
}

fn conductor_prompt(user: &UserEntry) -> bool {
    opens_turn(user) && !starts_with_structural_noise(pystr::strip(&user.content.text()))
}

fn classifier(id: &str) -> Value {
    json!({"id": id, "version": "1"})
}

pub fn failure(request_id: &str, status: &str, reason: &str, usage: Option<&Value>) -> Value {
    let mut counts = sonic_rs::Object::new();
    for key in USAGE_KEYS.iter() {
        counts.insert(key, json!(0));
    }
    if let Some(usage) = usage.and_then(|usage| usage.as_object()) {
        for (key, value) in usage.iter() {
            counts.insert(key, value.clone());
        }
    }
    let counter = if status == "cancelled" {
        "requests_cancelled"
    } else {
        "requests_failed"
    };
    let current = counts
        .get(&counter)
        .and_then(|value| value.as_u64())
        .unwrap_or(0);
    counts.insert(counter, json!(current.max(1)));
    json!({
        "schema": CORE_SCHEMA,
        "id": request_id,
        "status": status,
        "complete": false,
        "data": null,
        "cursor": null,
        "reason": reason.chars().take(4096).collect::<String>(),
        "usage": counts,
    })
}

impl Owner {
    pub fn new(store: NativeStore) -> Result<Self, SnapshotError> {
        let lane: Arc<ClassifierCallback> =
            Arc::new(|chunks: &[Arc<EntryChunk>], range: Range<usize>| {
                Ok(user_events(chunks, range, lane_prompt))
            });
        let conductor: Arc<ClassifierCallback> =
            Arc::new(|chunks: &[Arc<EntryChunk>], range: Range<usize>| {
                Ok(user_events(chunks, range, conductor_prompt))
            });
        store.register_classifier("captain-lane", "1", lane)?;
        store.register_classifier("captain-conductor", "1", conductor)?;
        Ok(Self {
            store,
            registry_generations: Mutex::new(Vec::new()),
            started: Instant::now(),
            last_refusal_log_ms: AtomicU64::new(0),
        })
    }

    fn registry_key(tool_registry: &Value, context: &Value) -> Result<RegistryKey, SnapshotError> {
        let mut entries = tool_registry
            .as_array()
            .ok_or_else(mismatch)?
            .iter()
            .map(|entry| pyjson::dumps(entry, CANONICAL).map_err(invalid))
            .collect::<Result<Vec<_>, _>>()?;
        entries.sort();
        let authority = context.get("authority").ok_or_else(mismatch)?;
        Ok((
            format!(
                "{:x}",
                Sha256::digest(format!("[{}]", entries.join(",")).as_bytes())
            ),
            text(context, "admission")?.to_owned(),
            pyjson::dumps(authority, CANONICAL).map_err(invalid)?,
        ))
    }

    fn registry_generation(
        &self,
        tool_registry: &Value,
        context: &Value,
    ) -> Result<String, SnapshotError> {
        let key = Self::registry_key(tool_registry, context)?;
        {
            let mut generations = self
                .registry_generations
                .lock()
                .expect("registry generations");
            if let Some(at) = generations.iter().position(|(known, _)| *known == key) {
                let entry = generations.remove(at);
                let generation = entry.1.clone();
                generations.push(entry);
                return Ok(generation);
            }
        }
        let generation = self
            .store
            .register_tool_registry(&native_input(tool_registry)?, &native_input(context)?)?;
        let mut generations = self
            .registry_generations
            .lock()
            .expect("registry generations");
        generations.retain(|(known, _)| *known != key);
        generations.push((key, generation.clone()));
        if generations.len() > MAX_REGISTRY_GENERATIONS {
            generations.remove(0);
        }
        Ok(generation)
    }

    pub fn call(
        &self,
        request: &Value,
        mut context: Value,
        cancel: &Cancellation,
        tool_registry: &Value,
        queued: Duration,
    ) -> Value {
        let started = Instant::now();
        let request_id = request.get("id").and_then(|id| id.as_str()).unwrap_or("");
        let (outcome, waits) = RequestWaits::measured(|| {
            self.registry_generation(tool_registry, &context)
                .map_err(Failure::from)
                .and_then(|generation| {
                    context
                        .as_object_mut()
                        .ok_or_else(|| Failure::from(mismatch()))?
                        .insert("registry_generation", json!(generation));
                    self.dispatch(request, &native_input(&context)?, cancel)
                })
        });
        match outcome {
            Ok((response, path)) => self.completed(request, response, started, path, queued, waits),
            Err(Failure { error, usage }) => {
                let mut fields = log::metadata(request);
                fields.push(("status", Field::Json(json!(error.status.as_str()))));
                fields.push(("elapsed_ms", Field::Float(ms(started.elapsed()))));
                fields.extend(wait_fields(queued, waits));
                match usage.as_ref().and_then(|usage| usage.as_object()) {
                    Some(counts) => {
                        for key in USAGE_KEYS.iter() {
                            if let Some(value) = counts.get(&key.as_str()) {
                                fields.push((key.as_str(), Field::Json(value.clone())));
                            }
                        }
                    }
                    None => fields.push(("usage", Field::Json(json!("unknown")))),
                }
                fields.push(("execution_path", Field::Json(json!("unknown_error"))));
                fields.push(("cursor_in_sha256", cursor_digest(request.get("cursor"))));
                fields.push(("cursor_out_sha256", Field::Json(json!(null))));
                log::info("snapshot request failed", &fields);
                failure(
                    request_id,
                    error.status.as_str(),
                    &error.reason,
                    usage.as_ref(),
                )
            }
        }
    }

    fn dispatch(
        &self,
        request: &Value,
        context: &Value,
        cancel: &Cancellation,
    ) -> Result<(Value, ExecutionPath), Failure> {
        let id = text(request, "id")?;
        let operation = text(request, "operation")?;
        if text(request, "schema")? == CORE_SCHEMA {
            if operation == "resume" {
                let cursor = text(request, "cursor")?;
                if cursor.len() > 4096 {
                    return Err(invalid("projection cursor exceeds byte limit").into());
                }
                if let Some(reply) = self.store.resume_projection(cursor, context, cancel)? {
                    return Ok((
                        page(&parsed(reply.json()), id),
                        ExecutionPath::OwnedProjection,
                    ));
                }
            }
            let response = self
                .store
                .request_agent(&native_input(request)?, context, cancel);
            snapshot_codec::encoded_size(&response, MAX_PAGE_BYTES)?;
            return Ok((response, ExecutionPath::CoreRequest));
        }
        let response = match operation {
            "prepare_classifier" => classification(
                &self.store.prepare_classifier(
                    &native_input(request.get("handle").ok_or_else(mismatch)?)?,
                    &native_input(request.get("classifier").ok_or_else(mismatch)?)?,
                    context,
                    cancel,
                    scope_limits(
                        request.get("limits").ok_or_else(mismatch)?,
                        request.get("deadline_unix_ms").ok_or_else(mismatch)?,
                    )?,
                )?,
                id,
            )?,
            "submit_classifier" => {
                let cursor = text(request, "cursor")?;
                let labels = request
                    .get("labels")
                    .and_then(|labels| labels.as_array())
                    .ok_or_else(mismatch)?;
                if labels.len() > MAX_RECORDS {
                    return Err(invalid("classifier label page exceeds item limit").into());
                }
                if cursor.len() > 4096 {
                    return Err(invalid("classifier cursor exceeds byte limit").into());
                }
                cancel.check(u64::MAX)?;
                let labels = labels
                    .iter()
                    .map(|label| label.as_bool().ok_or_else(mismatch))
                    .collect::<Result<Vec<_>, _>>()?;
                classification(
                    &self
                        .store
                        .submit_classifier(cursor, &labels, context, cancel)?,
                    id,
                )?
            }
            "prepare_hook_view" => {
                let mut scope = self.borrow(request, context, cancel)?;
                let classifier = hook_classifier(&mut scope, request)?;
                json!({
                    "schema": CORE_SCHEMA,
                    "id": id,
                    "status": "ok",
                    "complete": true,
                    "data": {"kind": "classifier", "classifier": classifier},
                    "cursor": null,
                    "reason": null,
                    "usage": scope.usage(),
                })
            }
            "review_events" => review_events::serve(&self.store, request, context, cancel, id)?,
            _ => return Err(invalid("unregistered Captain evidence policy").into()),
        };
        Ok((response, ExecutionPath::OtherOperation))
    }

    fn borrow<'a>(
        &'a self,
        request: &Value,
        context: &'a Value,
        cancel: &'a Cancellation,
    ) -> Result<Scope<'a>, SnapshotError> {
        let handle = request
            .get("view")
            .and_then(|view| view.get("handle"))
            .ok_or_else(mismatch)?;
        Scope::borrow(
            &self.store,
            native_input(handle)?,
            context,
            scope_limits(
                request.get("limits").ok_or_else(mismatch)?,
                request.get("deadline_unix_ms").ok_or_else(mismatch)?,
            )?,
            cancel,
        )
    }

    fn completed(
        &self,
        request: &Value,
        response: Value,
        started: Instant,
        path: ExecutionPath,
        queued: Duration,
        waits: RequestWaits,
    ) -> Value {
        let elapsed = ms(started.elapsed());
        if response["status"].as_str() == Some("retained_limit") {
            self.log_admission_refusal();
        }
        let usage = &response["usage"];
        let paged = request
            .get("cursor")
            .is_some_and(|cursor| !cursor.is_null())
            || !response["cursor"].is_null();
        let slow = elapsed + ms(queued) >= SLOW_REQUEST_MS;
        let read = log::truthy(usage.get("source_bytes_read"))
            || log::truthy(usage.get("prepared_cache_bytes_read"));
        if slow || (read && !paged) {
            let mut fields = log::metadata(request);
            fields.push(("pid", Field::Json(json!(std::process::id()))));
            fields.push(("status", Field::Json(lookup(&response, "status"))));
            fields.push(("elapsed_ms", Field::Float(elapsed)));
            fields.extend(wait_fields(queued, waits));
            fields.push((
                "query_kind",
                Field::Json(
                    request
                        .get("query")
                        .map_or(json!(null), |query| lookup(query, "kind")),
                ),
            ));
            fields.push((
                "observer_kinds",
                Field::Strings(
                    request
                        .get("observers")
                        .and_then(|observers| observers.as_array())
                        .map(|observers| {
                            observers
                                .iter()
                                .map(|observer| {
                                    observer["kind"].as_str().unwrap_or_default().to_owned()
                                })
                                .collect()
                        })
                        .unwrap_or_default(),
                ),
            ));
            fields.push((
                "selector_count",
                Field::Json(json!(request
                    .get("selectors")
                    .and_then(|selectors| selectors.as_array())
                    .map_or(0, |selectors| selectors.len()))),
            ));
            fields.push(("tail_bytes", Field::Json(lookup(request, "tail_bytes"))));
            for key in LOGGED_USAGE {
                fields.push((key, Field::Json(lookup(usage, key))));
            }
            fields.push(("execution_path", Field::Json(json!(path.as_str()))));
            fields.push(("cursor_in_sha256", cursor_digest(request.get("cursor"))));
            fields.push(("cursor_out_sha256", cursor_digest(response.get("cursor"))));
            log::info("snapshot request completed", &fields);
        }
        response
    }

    fn log_admission_refusal(&self) {
        let now = self.started.elapsed().as_millis() as u64;
        let last = self.last_refusal_log_ms.load(Ordering::Relaxed);
        if now < last.saturating_add(REFUSAL_LOG_INTERVAL_MS)
            || self
                .last_refusal_log_ms
                .compare_exchange(last, now.max(1), Ordering::Relaxed, Ordering::Relaxed)
                .is_err()
        {
            return;
        }
        let Some(report) = self.store.take_admission_refusal() else {
            return;
        };
        let mut fields = vec![("pid", Field::Json(json!(std::process::id())))];
        for key in ADMISSION_REFUSAL_FIELDS {
            fields.push((key, Field::Json(lookup(&report, key))));
        }
        fields.push((
            "heap_in_use_bytes",
            Field::Json(json!(log::heap_in_use_bytes())),
        ));
        log::info("snapshot admission refused", &fields);
    }

    pub fn discard(&self, response: &Value, context: &Value) -> Result<(), SnapshotError> {
        self.store
            .discard_response(response, &native_input(context)?)
            .map(|_| ())
    }
}

fn ms(duration: Duration) -> f64 {
    (duration.as_secs_f64() * 10_000.0).round() / 10.0
}

fn wait_fields(queued: Duration, waits: RequestWaits) -> [(&'static str, Field); 3] {
    [
        ("queue_ms", Field::Float(ms(queued))),
        ("state_wait_ms", Field::Float(ms(waits.state))),
        ("fact_read_wait_ms", Field::Float(ms(waits.fact_read))),
    ]
}

fn hook_classifier(scope: &mut Scope<'_>, request: &Value) -> Result<Value, Failure> {
    let description = scope.description()?;
    let path = description["canonical_path"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    if is_subagent_path(Path::new(&path)) {
        return Ok(classifier("captain-lane"));
    }
    let facts = scope.classifier_facts("<system_instruction>", 50)?;
    if facts["has_users"].as_bool() == Some(true)
        && facts["all_users_sidechain"].as_bool() == Some(true)
    {
        return Ok(classifier("captain-lane"));
    }
    let conductor = request["cwd"]
        .as_str()
        .is_some_and(|cwd| cwd.contains("conductor/workspaces"))
        || path.contains("conductor-workspaces")
        || facts["has_user_prefix"].as_bool() == Some(true);
    if conductor && request["droid"].as_bool() != Some(true) {
        return Ok(classifier("captain-conductor"));
    }
    Ok(classifier("native"))
}

fn classification(page: &Value, id: &str) -> Result<Value, SnapshotError> {
    snapshot_codec::encoded_size(page, MAX_PAGE_BYTES)?;
    let complete = page["complete"].as_bool() == Some(true);
    let data = if complete {
        json!({"kind": "acquired", "description": page["description"]})
    } else {
        json!({
            "kind": "classification",
            "record_schema": "cc-transcript.event/1",
            "records_json": page["records_json"],
            "event_start": page["event_start"],
        })
    };
    Ok(json!({
        "schema": CORE_SCHEMA,
        "id": id,
        "status": if complete { "ok" } else { "incomplete" },
        "complete": complete,
        "data": data,
        "cursor": page["cursor"],
        "reason": if complete { json!(null) } else { json!("caller classification required") },
        "usage": page["usage"],
    }))
}

fn page(reply: &Value, id: &str) -> Value {
    let field = reply["field"].as_str().expect("owned projection field");
    let records = if field == "corrections" {
        let mut records = sonic_rs::Array::new();
        for record in reply["records_json"]
            .as_array()
            .expect("owned records")
            .iter()
        {
            records.push(parsed(record.as_str().expect("owned record")));
        }
        records.into_value()
    } else {
        reply["records_json"].clone()
    };
    let mut data = reply["metadata"]
        .as_object()
        .expect("owned metadata")
        .clone();
    data.insert(&field, records);
    let complete = reply["complete"].as_bool() == Some(true);
    json!({
        "schema": CORE_SCHEMA,
        "id": id,
        "status": if complete { "ok" } else { "incomplete" },
        "complete": complete,
        "data": data,
        "cursor": reply["cursor"],
        "reason": if complete { json!(null) } else { json!("projection page bound") },
        "usage": reply["usage"],
    })
}
