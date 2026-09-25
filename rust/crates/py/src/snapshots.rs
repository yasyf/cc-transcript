use std::sync::{Arc, LazyLock};
use std::time::Instant;

use cc_transcript_core::snapshot::{
    Cancellation, NativeStore, Projection, SnapshotError, Status, TranscriptSnapshot, WorkLimits,
    SCHEMA,
};
use cc_transcript_core::{snapshot_codec, snapshot_projection};
use jsonschema::Validator;
use pyo3::exceptions::{PyIndexError, PyRuntimeError};
use pyo3::prelude::*;
use pyo3::types::PyTuple;
use serde_json::Value as SchemaValue;
use sonic_rs::{json, JsonContainerTrait, JsonValueTrait, Value};

use crate::mining;
use crate::snapshot_decode::{snapshot_activity_payload, ActivityUsage};
use crate::views::events::event_view;

pyo3::create_exception!(_native, SnapshotOperationError, PyRuntimeError);

const MAX_INPUT_BYTES: usize = snapshot_codec::MAX_PAGE_BYTES;
const REQUEST_SCHEMA: &str =
    include_str!("../../../../cc_transcript/snapshot_schema/request.schema.json");

struct Schemas {
    request: Validator,
    context: Validator,
    config: Validator,
    handle: Validator,
    classifier: Validator,
    anchor: Validator,
    render: Validator,
    scope_limits: Validator,
}

fn compile(schema: &SchemaValue) -> Validator {
    jsonschema::validator_for(schema).expect("embedded snapshot schema is valid")
}

static SCHEMAS: LazyLock<Schemas> = LazyLock::new(|| {
    let request: SchemaValue = serde_json::from_str(REQUEST_SCHEMA).expect("request schema JSON");
    let definition = |name: &str| {
        compile(&serde_json::json!({
            "$schema": request["$schema"], "$defs": request["$defs"],
            "$ref": format!("#/$defs/{name}")
        }))
    };
    Schemas {
        request: compile(&request),
        context: compile(
            &serde_json::from_str(include_str!(
                "../../../../cc_transcript/snapshot_schema/context.schema.json"
            ))
            .expect("context schema JSON"),
        ),
        config: compile(
            &serde_json::from_str(include_str!(
                "../../../../cc_transcript/snapshot_schema/config.schema.json"
            ))
            .expect("config schema JSON"),
        ),
        handle: definition("Handle"),
        classifier: definition("Classifier"),
        anchor: definition("EventRef"),
        render: definition("HydrationBudget"),
        scope_limits: compile(&serde_json::json!({
            "$schema": request["$schema"], "$defs": request["$defs"],
            "type": "object", "additionalProperties": false,
            "required": ["limits", "deadline_unix_ms"],
            "properties": {
                "limits": {"$ref":"#/$defs/Limits"},
                "deadline_unix_ms": request["$defs"]["Acquire"]["properties"]["deadline_unix_ms"]
            }
        })),
    }
});

pub(crate) fn error(error: SnapshotError) -> PyErr {
    SnapshotOperationError::new_err((error.status.as_str(), error.reason))
}

fn core_failure(py: Python<'_>, failure: PyErr) -> SnapshotError {
    if failure.is_instance_of::<SnapshotOperationError>(py) {
        if let Ok((status, reason)) = failure
            .value(py)
            .getattr("args")
            .and_then(|args| args.extract::<(String, String)>())
        {
            let status = match status.as_str() {
                "incomplete" => Status::Incomplete,
                "missing" => Status::Missing,
                "changed" => Status::Changed,
                "source_limit" => Status::SourceLimit,
                "entry_limit" => Status::EntryLimit,
                "retained_limit" => Status::RetainedLimit,
                "lease_limit" => Status::LeaseLimit,
                "output_limit" => Status::OutputLimit,
                "deadline" => Status::Deadline,
                "cancelled" => Status::Cancelled,
                "parse_error" => Status::ParseError,
                "permission_denied" => Status::PermissionDenied,
                "stale_handle" => Status::StaleHandle,
                "stale_cursor" => Status::StaleCursor,
                _ => Status::InvalidRequest,
            };
            return SnapshotError::new(status, reason);
        }
    }
    invalid(format!("mining policy failed: {failure}"))
}

fn invalid(reason: impl Into<String>) -> SnapshotError {
    SnapshotError::new(Status::InvalidRequest, reason)
}

fn parse(text: &str, limit: usize) -> Result<Value, SnapshotError> {
    if text.len() > limit.min(MAX_INPUT_BYTES) {
        return Err(SnapshotError::new(
            Status::SourceLimit,
            "JSON input exceeds byte limit",
        ));
    }
    sonic_rs::from_str(text).map_err(|_| invalid("malformed JSON input"))
}

fn validate(value: &Value, schema: &Validator) -> Result<(), SnapshotError> {
    let instance = serde_json::to_value(value).map_err(|_| invalid("invalid JSON value"))?;
    schema
        .validate(&instance)
        .map_err(|_| invalid("input does not match snapshot schema"))
}

fn exact_integer(text: &str) -> Result<i64, SnapshotError> {
    const MAX_SAFE: u64 = 9_007_199_254_740_991;
    let (negative, unsigned) = text
        .strip_prefix('-')
        .map_or((false, text), |rest| (true, rest));
    let (coefficient, exponent) = unsigned.split_once(['e', 'E']).unwrap_or((unsigned, "0"));
    let fractional = coefficient
        .split_once('.')
        .map_or(0, |(_, fraction)| fraction.len());
    let digits: String = coefficient.chars().filter(|&digit| digit != '.').collect();
    let digits = digits.trim_start_matches('0');
    if digits.is_empty() {
        return Ok(0);
    }
    let significant = digits.trim_end_matches('0');
    let trailing = digits.len() - significant.len();
    let exponent: i64 = exponent
        .parse()
        .map_err(|_| invalid("integer exponent exceeds safe range"))?;
    let scale = exponent
        .checked_add(trailing as i64)
        .and_then(|value| value.checked_sub(fractional as i64))
        .ok_or_else(|| invalid("integer exponent exceeds safe range"))?;
    if scale < 0 {
        return Err(invalid("fractional metadata number"));
    }
    if scale > 16 || significant.len().saturating_add(scale as usize) > 16 {
        return Err(invalid("metadata integer exceeds safe range"));
    }
    let mut value: u64 = significant
        .parse()
        .map_err(|_| invalid("invalid metadata integer"))?;
    for _ in 0..scale {
        value *= 10;
    }
    if value > MAX_SAFE {
        return Err(invalid("metadata integer exceeds safe range"));
    }
    Ok(if negative {
        -(value as i64)
    } else {
        value as i64
    })
}

fn normalize_numbers(value: &mut SchemaValue) -> Result<(), SnapshotError> {
    match value {
        SchemaValue::Array(values) => {
            for value in values {
                normalize_numbers(value)?;
            }
        }
        SchemaValue::Object(values) => {
            for value in values.values_mut() {
                normalize_numbers(value)?;
            }
        }
        SchemaValue::Number(number) => {
            *number = serde_json::Number::from(exact_integer(&number.to_string())?);
        }
        _ => {}
    }
    Ok(())
}

fn native_value(value: &SchemaValue) -> Value {
    match value {
        SchemaValue::Null => json!(null),
        SchemaValue::Bool(value) => json!(value),
        SchemaValue::String(value) => json!(value),
        SchemaValue::Number(value) => json!(value.as_i64().expect("normalized metadata integer")),
        SchemaValue::Array(values) => {
            let mut array = sonic_rs::Array::with_capacity(values.len());
            for value in values {
                array.push(native_value(value));
            }
            array.into_value()
        }
        SchemaValue::Object(values) => {
            let mut object = sonic_rs::Object::with_capacity(values.len());
            for (key, value) in values {
                object.insert(key.as_str(), native_value(value));
            }
            object.into_value()
        }
    }
}

fn validated(text: &str, schema: &Validator) -> Result<Value, SnapshotError> {
    if text.len() > MAX_INPUT_BYTES {
        return Err(SnapshotError::new(
            Status::SourceLimit,
            "JSON input exceeds byte limit",
        ));
    }
    let mut value: SchemaValue =
        serde_json::from_str(text).map_err(|_| invalid("malformed JSON input"))?;
    normalize_numbers(&mut value)?;
    schema
        .validate(&value)
        .map_err(|_| invalid("input does not match snapshot schema"))?;
    Ok(native_value(&value))
}

fn count(value: &Value, key: &str) -> usize {
    usize::try_from(
        value
            .get(key)
            .and_then(Value::as_u64)
            .expect("validated integer"),
    )
    .expect("snapshot integer fits platform size")
}

fn scope_limits(value: &Value) -> WorkLimits {
    let limits = value.get("limits").expect("validated scope limits");
    WorkLimits {
        max_read_bytes: count(limits, "max_read_bytes"),
        max_events: count(limits, "max_events"),
        max_items: count(limits, "max_items"),
        max_output_bytes: count(limits, "max_output_bytes"),
        max_discovery_entries: count(limits, "max_discovery_entries"),
        max_sources: count(limits, "max_sources"),
        deadline_unix_ms: value
            .get("deadline_unix_ms")
            .and_then(Value::as_u64)
            .expect("validated deadline"),
    }
}

fn limits_value(limits: WorkLimits) -> Value {
    json!({
        "max_read_bytes":limits.max_read_bytes,"max_events":limits.max_events,
        "max_items":limits.max_items,"max_output_bytes":limits.max_output_bytes,
        "max_discovery_entries":limits.max_discovery_entries,"max_sources":limits.max_sources
    })
}

#[pyo3_stub_gen::derive::gen_stub_pyclass]
#[pyclass(
    name = "SnapshotCancellation",
    module = "cc_transcript._native",
    frozen
)]
pub(crate) struct SnapshotCancellation {
    inner: Cancellation,
}

#[pyo3_stub_gen::derive::gen_stub_pymethods]
#[pymethods]
impl SnapshotCancellation {
    #[new]
    fn new() -> Self {
        Self {
            inner: Cancellation::default(),
        }
    }

    fn cancel(&self) {
        self.inner.cancel();
    }
}

#[pyo3_stub_gen::derive::gen_stub_pyclass]
#[pyclass(name = "NativeSnapshotStore", module = "cc_transcript._native", frozen)]
pub(crate) struct NativeSnapshotStore {
    inner: Arc<NativeStore>,
}

#[pyo3_stub_gen::derive::gen_stub_pymethods]
#[pymethods]
impl NativeSnapshotStore {
    #[new]
    fn new(py: Python<'_>, config_json: &str) -> PyResult<Self> {
        py.detach(|| {
            let config = validated(config_json, &SCHEMAS.config)?;
            NativeStore::new(&config).map(|inner| Self {
                inner: Arc::new(inner),
            })
        })
        .map_err(error)
    }

    fn owner_epoch(&self) -> String {
        self.inner.owner_epoch.clone()
    }

    fn record_transport(&self, py: Python<'_>, bytes: usize) {
        py.detach(|| self.inner.record_transport(bytes));
    }

    fn request(
        &self,
        py: Python<'_>,
        request_json: &str,
        context_json: &str,
        cancel: &SnapshotCancellation,
    ) -> PyResult<String> {
        py.detach(|| {
            let request = validated(request_json, &SCHEMAS.request)?;
            let context = validated(context_json, &SCHEMAS.context)?;
            let response = self.inner.request(&request, &context, &cancel.inner);
            snapshot_codec::encode(&response, MAX_INPUT_BYTES)
        })
        .map_err(error)
    }

    fn borrow(
        &self,
        py: Python<'_>,
        handle_json: &str,
        context_json: &str,
        limits_json: &str,
        cancel: &SnapshotCancellation,
    ) -> PyResult<NativeSnapshotScope> {
        py.detach(|| {
            let handle = validated(handle_json, &SCHEMAS.handle)?;
            let context = validated(context_json, &SCHEMAS.context)?;
            let limits = scope_limits(&validated(limits_json, &SCHEMAS.scope_limits)?);
            cancel.inner.check(limits.deadline_unix_ms)?;
            let (snapshot, description) = self.inner.pin_scope(&handle, &context)?;
            cancel.inner.check(limits.deadline_unix_ms)?;
            Ok(NativeSnapshotScope {
                owner: Arc::clone(&self.inner),
                context,
                snapshot: Some(snapshot),
                description,
                handle,
                limits,
                cancel: cancel.inner.clone(),
                used: ScopeWork::default(),
                started: Instant::now(),
                materialized_output_bytes: 0,
                failures: 0,
                cancellations: 0,
            })
        })
        .map_err(error)
    }

    fn register_mining_policy(
        &self,
        py: Python<'_>,
        id: &str,
        version: &str,
        spec_json: &str,
        callable_formats: Vec<(String, Py<PyAny>, Py<PyAny>, bool)>,
    ) -> PyResult<()> {
        validate(&json!({"id":id,"version":version}), &SCHEMAS.classifier).map_err(error)?;
        if spec_json.len() > MAX_INPUT_BYTES {
            return Err(error(SnapshotError::new(
                Status::SourceLimit,
                "mining spec exceeds input limit",
            )));
        }
        let mut spec = py
            .detach(|| mining::compile_spec(spec_json))
            .map_err(|reason| error(invalid(reason)))?;
        mining::attach_bounded_callable_formats(&mut spec, callable_formats)?;
        let spec = Arc::new(spec);
        self.inner.register_policy(id, version, Arc::new(move |snapshot, request, limits, cancel, next| {
            if next != 0 {
                return Err(SnapshotError::new(Status::StaleCursor, "mining policy has no partial cursor"));
            }
            let view = request.get("view").ok_or_else(|| invalid("missing mining view"))?;
            if ["selectors", "attachments"].iter().any(|key| view.get(*key).and_then(Value::as_array).is_some_and(|values| !values.is_empty())) {
                return Err(SnapshotError::new(Status::Incomplete, "registered mining requires a complete source view"));
            }
            let mut limits = *limits;
            limits.max_items = limits.max_items.min(snapshot_codec::MAX_RECORDS);
            let mut usage = mining::MiningUsage::default();
            let records = Python::attach(|py| -> Result<Vec<String>, SnapshotError> {
                let payloads = mining::mine_snapshot(py, &snapshot, &spec, limits, cancel, &mut usage)
                    .map_err(|failure| core_failure(py, failure))?;
                let encoder = py.import("json").and_then(|module| module.getattr("dumps"))
                    .map_err(|failure| core_failure(py, failure))?;
                let mut remaining = limits.max_output_bytes;
                let mut records = Vec::with_capacity(payloads.len());
                for payload in payloads {
                    cancel.check(limits.deadline_unix_ms)?;
                    let record: String = encoder.call1((payload,)).and_then(|value| value.extract())
                        .map_err(|failure| core_failure(py, failure))?;
                    if record.len() > snapshot_codec::MAX_RECORD_BYTES || record.len() > remaining {
                        return Err(SnapshotError::new(Status::OutputLimit, "mining record exceeds output budget"));
                    }
                    remaining -= record.len();
                    records.push(record);
                }
                Ok(records)
            })?;
            let data = json!({"kind":"records","record_schema":"cc-transcript.mining-signal/1","records_json":records});
            snapshot_codec::encoded_size(&data, limits.max_output_bytes)?;
            Ok(Projection { read_bytes:usage.input_bytes, events:usage.events, items:usage.items,
                data, complete:true, next:None, reason:None })
        })).map_err(error)
    }

    fn register_classifier(
        &self,
        py: Python<'_>,
        id: &str,
        version: &str,
        callback: Py<PyAny>,
    ) -> PyResult<()> {
        if !callback.bind(py).is_callable() {
            return Err(error(invalid("classifier must be callable")));
        }
        validate(&json!({"id":id,"version":version}), &SCHEMAS.classifier).map_err(error)?;
        self.inner
            .register_classifier(
                id,
                version,
                Arc::new(move |chunks, range| {
                    Python::attach(|py| {
                        let events = range
                            .map(|position| {
                                let at =
                                    chunks.partition_point(|chunk| chunk.start <= position) - 1;
                                let chunk = &chunks[at];
                                event_view(py, &chunk.entries, position - chunk.start)
                            })
                            .collect::<PyResult<Vec<_>>>()?;
                        let events = PyTuple::new(py, events)?;
                        callback.bind(py).call1((events,))?.extract::<Vec<bool>>()
                    })
                    .map_err(|failure| invalid(format!("classifier callback failed: {failure}")))
                }),
            )
            .map_err(error)
    }
}

#[derive(Default, Clone, Copy)]
struct ScopeWork {
    read_bytes: usize,
    events: usize,
    items: usize,
    output_bytes: usize,
}

impl ScopeWork {
    fn charge(&mut self, limits: WorkLimits, additional: Self) -> Result<(), SnapshotError> {
        for (used, amount, maximum, reason) in [
            (
                self.read_bytes,
                additional.read_bytes,
                limits.max_read_bytes,
                "read_limit",
            ),
            (
                self.events,
                additional.events,
                limits.max_events,
                "event_limit",
            ),
            (self.items, additional.items, limits.max_items, "item_limit"),
            (
                self.output_bytes,
                additional.output_bytes,
                limits.max_output_bytes,
                "output_limit",
            ),
        ] {
            if amount > maximum.saturating_sub(used) {
                return Err(SnapshotError::new(Status::Incomplete, reason));
            }
        }
        self.read_bytes += additional.read_bytes;
        self.events += additional.events;
        self.items += additional.items;
        self.output_bytes += additional.output_bytes;
        Ok(())
    }

    fn remaining(self, limits: WorkLimits) -> WorkLimits {
        WorkLimits {
            max_read_bytes: limits.max_read_bytes.saturating_sub(self.read_bytes),
            max_events: limits.max_events.saturating_sub(self.events),
            max_items: limits.max_items.saturating_sub(self.items),
            max_output_bytes: limits.max_output_bytes.saturating_sub(self.output_bytes),
            ..limits
        }
    }
}

#[pyo3_stub_gen::derive::gen_stub_pyclass]
#[pyclass(name = "NativeSnapshotScope", module = "cc_transcript._native")]
pub(crate) struct NativeSnapshotScope {
    owner: Arc<NativeStore>,
    context: Value,
    materialized_output_bytes: usize,
    snapshot: Option<Arc<TranscriptSnapshot>>,
    description: Value,
    handle: Value,
    limits: WorkLimits,
    cancel: Cancellation,
    used: ScopeWork,
    started: Instant,
    failures: usize,
    cancellations: usize,
}

impl NativeSnapshotScope {
    fn checked_snapshot(&self, py: Python<'_>) -> Result<Arc<TranscriptSnapshot>, SnapshotError> {
        self.cancel.check(self.limits.deadline_unix_ms)?;
        py.detach(|| self.owner.validate_scope(&self.handle, &self.context))?;
        self.snapshot
            .as_ref()
            .cloned()
            .ok_or_else(|| SnapshotError::new(Status::StaleHandle, "borrow scope is closed"))
    }

    fn failure(&mut self, failure: SnapshotError) -> PyErr {
        self.failures += 1;
        if failure.status == Status::Cancelled {
            self.cancellations += 1;
        }
        SnapshotOperationError::new_err((
            failure.status.as_str(),
            failure.reason,
            self.usage(),
            self.work(),
        ))
    }

    fn python_failure(&mut self, py: Python<'_>, failure: PyErr) -> PyErr {
        self.failures += 1;
        if failure.is_instance_of::<SnapshotOperationError>(py) {
            if let Ok((status, reason)) = failure
                .value(py)
                .getattr("args")
                .and_then(|args| args.extract::<(String, String)>())
            {
                if status == "cancelled" {
                    self.cancellations += 1;
                }
                return SnapshotOperationError::new_err((
                    status,
                    reason,
                    self.usage(),
                    self.work(),
                ));
            }
        }
        let _ = failure.value(py).setattr("scope_usage", self.usage());
        let _ = failure.value(py).setattr("scope_work", self.work());
        failure
    }

    fn charge(&mut self, additional: ScopeWork) -> PyResult<()> {
        self.used
            .charge(self.limits, additional)
            .map_err(|failure| self.failure(failure))
    }

    fn project(&mut self, py: Python<'_>, request: Value, output_key: &str) -> PyResult<String> {
        let snapshot = self
            .checked_snapshot(py)
            .map_err(|failure| self.failure(failure))?;
        let limits = self.used.remaining(self.limits);
        if limits.max_events == 0
            || limits.max_items == 0
            || limits.max_read_bytes == 0
            || limits.max_output_bytes == 0
        {
            return Err(self.failure(SnapshotError::new(
                Status::Incomplete,
                "scope budget exhausted",
            )));
        }
        let mut usage = snapshot_projection::ProjectionUsage::default();
        let result = py.detach(|| {
            validate(&request, &SCHEMAS.request)?;
            snapshot_projection::project_with_usage(
                &snapshot,
                &request,
                &limits,
                &self.cancel,
                0,
                &mut usage,
            )
        });
        self.charge(ScopeWork {
            read_bytes: usage.read_bytes,
            events: usage.events,
            ..ScopeWork::default()
        })?;
        let result = result.map_err(|failure| self.failure(failure))?;
        self.charge(ScopeWork {
            items: result.items,
            ..ScopeWork::default()
        })?;
        if !result.complete {
            return Err(self.failure(SnapshotError::new(
                Status::Incomplete,
                result
                    .reason
                    .unwrap_or_else(|| "projection incomplete".to_owned()),
            )));
        }
        let encoded = py
            .detach(|| {
                snapshot_codec::encode(
                    result
                        .data
                        .get(output_key)
                        .expect("projection output field"),
                    limits.max_output_bytes,
                )
            })
            .map_err(|failure| self.failure(failure))?;
        self.materialized_output_bytes =
            self.materialized_output_bytes.saturating_add(encoded.len());
        self.cancel
            .check(self.limits.deadline_unix_ms)
            .map_err(|failure| self.failure(failure))?;
        Ok(encoded)
    }
}

#[pyo3_stub_gen::derive::gen_stub_pymethods]
#[pymethods]
impl NativeSnapshotScope {
    fn close(&mut self) {
        self.snapshot = None;
    }

    fn checkpoint(&mut self, py: Python<'_>) -> PyResult<()> {
        self.checked_snapshot(py)
            .map(|_| ())
            .map_err(|failure| self.failure(failure))
    }

    fn consume(
        &mut self,
        py: Python<'_>,
        events: usize,
        items: usize,
        output_bytes: usize,
    ) -> PyResult<()> {
        self.checkpoint(py)?;
        self.charge(ScopeWork {
            events,
            items,
            output_bytes,
            ..ScopeWork::default()
        })
    }

    fn usage(&self) -> String {
        json!({
            "source_opens":0,"source_bytes_read":0,"bytes_decoded":0,"events_parsed":0,
            "cold_parses":0,"append_parses":0,"activity_lifts":0,"cache_hits":0,"inflight_joins":0,
            "generations_published":0,"generations_invalidated":0,
            "requests_cancelled":self.cancellations,"requests_failed":self.failures,
            "output_bytes":self.used.output_bytes,"transport_bytes":0,
            "nonincremental_lowering_calls":0,"nonincremental_lowering_source_bytes":0,
            "discovery_entries_examined":0
        })
        .to_string()
    }

    fn work(&self) -> String {
        json!({"read_bytes":self.used.read_bytes,"events":self.used.events,"items":self.used.items,
            "output_bytes":self.used.output_bytes,"materialized_output_bytes":self.materialized_output_bytes,"elapsed_ms":self.started.elapsed().as_millis() as u64}).to_string()
    }

    fn description(&mut self, py: Python<'_>) -> PyResult<String> {
        self.checkpoint(py)?;
        let limit = self.used.remaining(self.limits).max_output_bytes;
        let encoded = py
            .detach(|| snapshot_codec::encode(&self.description, limit))
            .map_err(|failure| self.failure(failure))?;
        self.materialized_output_bytes =
            self.materialized_output_bytes.saturating_add(encoded.len());
        Ok(encoded)
    }

    fn event_count(&mut self, py: Python<'_>) -> PyResult<usize> {
        self.checked_snapshot(py)
            .map(|snapshot| snapshot.event_count)
            .map_err(|failure| self.failure(failure))
    }

    fn event<'py>(&mut self, py: Python<'py>, index: usize) -> PyResult<Bound<'py, PyAny>> {
        let snapshot = self
            .checked_snapshot(py)
            .map_err(|failure| self.failure(failure))?;
        if index >= snapshot.event_count {
            return Err(PyIndexError::new_err("event index out of range"));
        }
        let at = snapshot
            .chunks
            .partition_point(|chunk| chunk.start <= index)
            - 1;
        let chunk = &snapshot.chunks[at];
        let local = index - chunk.start;
        let charge = chunk.entry_charges[local];
        self.charge(ScopeWork {
            read_bytes: charge
                .owned_capacity_bytes
                .saturating_add(charge.opaque_dom_accounted_bytes),
            events: 1,
            items: 1,
            output_bytes: 0,
        })?;
        let limit = self.used.remaining(self.limits).max_output_bytes;
        let output_bytes = py
            .detach(|| {
                snapshot_codec::encoded_size(
                    &snapshot_codec::EventWire::new(index, &chunk.entries[local]),
                    limit,
                )
            })
            .map_err(|failure| self.failure(failure))?;
        self.materialized_output_bytes =
            self.materialized_output_bytes.saturating_add(output_bytes);
        self.checkpoint(py)?;
        event_view(py, &chunk.entries, local)
    }

    #[pyo3(signature = (classifier_json, anchor_json=None, lookback=40, lookahead=120))]
    fn activity<'py>(
        &mut self,
        py: Python<'py>,
        classifier_json: &str,
        anchor_json: Option<&str>,
        lookback: usize,
        lookahead: usize,
    ) -> PyResult<Bound<'py, pyo3::types::PyDict>> {
        let snapshot = self
            .checked_snapshot(py)
            .map_err(|failure| self.failure(failure))?;
        let classifier = py
            .detach(|| validated(classifier_json, &SCHEMAS.classifier))
            .map_err(|failure| self.failure(failure))?;
        if self.description.get("classifier") != Some(&classifier) {
            return Err(self.failure(invalid("classifier differs from pinned generation")));
        }
        let turns = match anchor_json {
            Some(text) => {
                let anchor = py
                    .detach(|| validated(text, &SCHEMAS.anchor))
                    .map_err(|failure| self.failure(failure))?;
                if anchor.get("session_id").and_then(Value::as_str)
                    != Some(snapshot.session_id.as_str())
                {
                    return Err(self.failure(invalid("activity anchor session mismatch")));
                }
                let uuid = anchor
                    .get("event_uuid")
                    .and_then(Value::as_str)
                    .expect("validated anchor");
                let turn = snapshot
                    .activity
                    .turn_of_uuid(uuid)
                    .ok_or_else(|| self.failure(invalid("activity anchor not found")))?;
                turn.saturating_sub(lookback)
                    ..turn
                        .saturating_add(lookahead)
                        .saturating_add(1)
                        .min(snapshot.activity.turn_count())
            }
            None => 0..snapshot.activity.turn_count(),
        };
        let limits = self.used.remaining(self.limits);
        let mut usage = ActivityUsage::default();
        let payload =
            snapshot_activity_payload(py, &snapshot, turns, &limits, &self.cancel, &mut usage);
        self.charge(ScopeWork {
            read_bytes: usage.read_bytes,
            events: usage.events,
            items: usage.items,
            output_bytes: 0,
        })?;
        self.materialized_output_bytes = self
            .materialized_output_bytes
            .saturating_add(usage.output_bytes);
        payload.map_err(|failure| self.python_failure(py, failure))
    }

    fn mine<'py>(
        &mut self,
        py: Python<'py>,
        spec_json: &str,
        callable_formats: Vec<(String, Py<PyAny>, Py<PyAny>, bool)>,
    ) -> PyResult<Vec<Bound<'py, pyo3::types::PyDict>>> {
        let snapshot = self
            .checked_snapshot(py)
            .map_err(|failure| self.failure(failure))?;
        let limits = self.used.remaining(self.limits);
        if spec_json.len() > limits.max_read_bytes.min(MAX_INPUT_BYTES) {
            return Err(self.failure(SnapshotError::new(
                Status::SourceLimit,
                "mining spec exceeds input budget",
            )));
        }
        self.charge(ScopeWork {
            read_bytes: spec_json.len(),
            ..ScopeWork::default()
        })?;
        let mut spec = py
            .detach(|| mining::compile_spec(spec_json))
            .map_err(|reason| self.failure(invalid(reason)))?;
        mining::attach_bounded_callable_formats(&mut spec, callable_formats)
            .map_err(|failure| self.python_failure(py, failure))?;
        let limits = self.used.remaining(self.limits);
        let mut usage = mining::MiningUsage::default();
        let result = mining::mine_snapshot(py, &snapshot, &spec, limits, &self.cancel, &mut usage);
        self.charge(ScopeWork {
            read_bytes: usage.input_bytes,
            events: usage.events,
            items: usage.items,
            output_bytes: 0,
        })?;
        self.materialized_output_bytes = self
            .materialized_output_bytes
            .saturating_add(usage.output_bytes);
        result.map_err(|failure| self.python_failure(py, failure))
    }

    #[pyo3(signature = (anchors_json, before=6, after=2, preview_chars=200))]
    fn capture(
        &mut self,
        py: Python<'_>,
        anchors_json: &str,
        before: usize,
        after: usize,
        preview_chars: usize,
    ) -> PyResult<String> {
        self.checkpoint(py)?;
        let limits = self.used.remaining(self.limits);
        let anchors = py
            .detach(|| parse(anchors_json, limits.max_read_bytes))
            .map_err(|failure| self.failure(failure))?;
        self.project(py, json!({
            "schema":SCHEMA,"id":"scope-capture","operation":"capture","deadline_unix_ms":limits.deadline_unix_ms,
            "limits":limits_value(limits), "view":{"handle":self.handle,"classifier":self.description.get("classifier").expect("trusted classifier"),"selectors":[],"attachments":[]},
            "anchors":anchors,"before":before,"after":after,"preview_chars":preview_chars
        }), "windows_json")
    }

    fn hydrate(
        &mut self,
        py: Python<'_>,
        windows_json: &str,
        render_json: &str,
    ) -> PyResult<String> {
        self.checkpoint(py)?;
        let limits = self.used.remaining(self.limits);
        let windows = py
            .detach(|| parse(windows_json, limits.max_read_bytes))
            .map_err(|failure| self.failure(failure))?;
        if render_json.len() > limits.max_read_bytes {
            return Err(self.failure(SnapshotError::new(
                Status::SourceLimit,
                "render parameters exceed input budget",
            )));
        }
        let render = py
            .detach(|| validated(render_json, &SCHEMAS.render))
            .map_err(|failure| self.failure(failure))?;
        let snapshot = self
            .checked_snapshot(py)
            .map_err(|failure| self.failure(failure))?;
        self.project(py, json!({
            "schema":SCHEMA,"id":"scope-hydrate","operation":"hydrate","deadline_unix_ms":limits.deadline_unix_ms,
            "limits":limits_value(limits), "handles":[{"session_id":snapshot.session_id,"handle":self.handle}],
            "windows_json":windows,"render":render
        }), "windows")
    }
}

pub(crate) fn add_functions(module: &Bound<'_, PyModule>) -> PyResult<()> {
    module.add(
        "SnapshotOperationError",
        module.py().get_type::<SnapshotOperationError>(),
    )?;
    module.add_class::<SnapshotCancellation>()?;
    module.add_class::<NativeSnapshotStore>()?;
    module.add_class::<NativeSnapshotScope>()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits() -> WorkLimits {
        WorkLimits {
            max_read_bytes: 10,
            max_events: 2,
            max_items: 2,
            max_output_bytes: 10,
            max_discovery_entries: 1,
            max_sources: 1,
            deadline_unix_ms: u64::MAX,
        }
    }

    #[test]
    fn cumulative_budget_rejects_before_mutating_usage() {
        let mut used = ScopeWork::default();
        used.charge(
            limits(),
            ScopeWork {
                read_bytes: 8,
                events: 1,
                items: 1,
                output_bytes: 6,
            },
        )
        .unwrap();
        assert_eq!(used.remaining(limits()).max_read_bytes, 2);
        let failure = used
            .charge(
                limits(),
                ScopeWork {
                    read_bytes: 3,
                    ..ScopeWork::default()
                },
            )
            .unwrap_err();
        assert_eq!(failure.reason, "read_limit");
        assert_eq!(used.read_bytes, 8);
        assert_eq!(used.output_bytes, 6);
    }

    #[test]
    fn schemas_reject_unknown_fields_and_boolean_integers() {
        assert!(validated(r#"{"unknown":1}"#, &SCHEMAS.config).is_err());
        assert!(validated(r#"{"max_entry_bytes":true}"#, &SCHEMAS.config).is_err());
        assert!(validated(r#"{}"#, &SCHEMAS.config).is_ok());
        assert!(validated(
            r#"{"id":"native","version":"1","extra":false}"#,
            &SCHEMAS.classifier
        )
        .is_err());
    }

    #[test]
    fn integral_json_numbers_normalize_without_touching_encoded_records() {
        for input in [r#"{"max_entry_bytes":1.0}"#, r#"{"max_entry_bytes":1e0}"#] {
            let value = validated(input, &SCHEMAS.config).unwrap();
            assert_eq!(
                value.get("max_entry_bytes").and_then(Value::as_u64),
                Some(1)
            );
        }
        let mut value = serde_json::json!({"record_json":"{\"n\":1.0}","n":1.0});
        normalize_numbers(&mut value).unwrap();
        assert_eq!(value["record_json"].as_str(), Some("{\"n\":1.0}"));
        assert_eq!(value["n"].as_u64(), Some(1));
    }

    #[test]
    fn lexical_fraction_and_safe_integer_limits_are_exact() {
        for token in [
            "9007199254740990.5",
            "1.00000000000000001",
            "9007199254740992",
            "1e1000000",
            "1e-1000000",
            "NaN",
        ] {
            let input = format!("{{\"max_entry_bytes\":{token}}}");
            assert!(validated(&input, &SCHEMAS.config).is_err(), "{token}");
        }
        for (token, expected) in [
            ("1.0", 1),
            ("1e0", 1),
            ("1000e-3", 1),
            ("9.007199254740991e15", 9_007_199_254_740_991),
            ("0e999999999999999999999", 0),
            ("-0.0e-999999999999999999999", 0),
        ] {
            let mut value: SchemaValue = serde_json::from_str(token).unwrap();
            normalize_numbers(&mut value).unwrap();
            assert_eq!(value.as_i64(), Some(expected), "{token}");
        }
    }

    #[test]
    fn input_budget_is_checked_before_json_parsing() {
        let failure = parse("not json", 1).unwrap_err();
        assert_eq!(failure.status, Status::SourceLimit);
    }

    #[test]
    fn cancellation_precedes_deadline_and_is_shared() {
        let token = Cancellation::default();
        let other = token.clone();
        other.cancel();
        assert_eq!(token.check(0).unwrap_err().status, Status::Cancelled);
    }
    #[test]
    fn owner_scope_preflights_views_and_observes_release() {
        Python::initialize();
        Python::attach(|py| {
            static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            let directory = std::env::temp_dir().join(format!(
                "cc-snapshot-bridge-{}-{}-{}",
                std::process::id(),
                cc_transcript_core::snapshot::now_ms(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            std::fs::create_dir(&directory).unwrap();
            let path = directory.join("s.jsonl");
            std::fs::write(&path, concat!(
                r#"{"type":"user","uuid":"u","sessionId":"s","timestamp":"2026-01-02T03:04:05Z","message":{"content":"preserve bounded owner view"}}"#,
                "\n"
            )).unwrap();
            let uid: u64 = py
                .import("os")
                .unwrap()
                .call_method0("geteuid")
                .unwrap()
                .extract()
                .unwrap();
            let context = json!({"claimant":"test","admission":"hook","authority":{"kind":"user","effective_uid":uid.to_string()},"registry_generation":"1"}).to_string();
            let deadline = cc_transcript_core::snapshot::now_ms() + 30_000;
            let generous = json!({"max_read_bytes":1024*1024,"max_events":100,"max_items":100,"max_output_bytes":1024*1024,"max_discovery_entries":100,"max_sources":10});
            let cancellation = SnapshotCancellation::new();
            let store = NativeSnapshotStore::new(py, "{}").unwrap();
            let request = json!({"schema":SCHEMA,"id":"acquire","operation":"acquire","path":path.to_string_lossy().as_ref(),"classifier":{"id":"native","version":"1"},"limits":generous,"deadline_unix_ms":deadline});
            let mut response: Value = sonic_rs::from_str(
                &store
                    .request(py, &request.to_string(), &context, &cancellation)
                    .unwrap(),
            )
            .unwrap();
            for _ in 0..100 {
                if response.get("status").and_then(Value::as_str) != Some("incomplete") {
                    break;
                }
                let resume = json!({"schema":SCHEMA,"id":"resume","operation":"resume","cursor":response.get("cursor").unwrap()});
                response = sonic_rs::from_str(
                    &store
                        .request(py, &resume.to_string(), &context, &cancellation)
                        .unwrap(),
                )
                .unwrap();
            }
            assert_eq!(
                response.get("status").and_then(Value::as_str),
                Some("ok"),
                "{response:?}"
            );
            let handle = response
                .get("data")
                .unwrap()
                .get("description")
                .unwrap()
                .get("handle")
                .unwrap()
                .to_string();
            let mut tiny = generous.clone();
            tiny.insert("max_read_bytes", json!(1));
            let mut scope = store
                .borrow(
                    py,
                    &handle,
                    &context,
                    &json!({"limits":tiny,"deadline_unix_ms":deadline}).to_string(),
                    &cancellation,
                )
                .unwrap();
            let count = Arc::strong_count(&scope.snapshot.as_ref().unwrap().chunks[0].entries);
            let failure = scope.event(py, 0).unwrap_err();
            assert!(failure.is_instance_of::<SnapshotOperationError>(py));
            assert_eq!(scope.used.events, 0);
            assert_eq!(
                Arc::strong_count(&scope.snapshot.as_ref().unwrap().chunks[0].entries),
                count
            );
            scope.close();
            let mut scope = store
                .borrow(
                    py,
                    &handle,
                    &context,
                    &json!({"limits":generous,"deadline_unix_ms":deadline}).to_string(),
                    &cancellation,
                )
                .unwrap();
            let event = scope.event(py, 0).unwrap();
            let release = json!({"schema":SCHEMA,"id":"release","operation":"release","handle":sonic_rs::from_str::<Value>(&handle).unwrap()});
            store
                .request(py, &release.to_string(), &context, &cancellation)
                .unwrap();
            assert!(scope
                .checkpoint(py)
                .unwrap_err()
                .is_instance_of::<SnapshotOperationError>(py));
            scope.close();
            assert_eq!(
                event.getattr("text").unwrap().extract::<String>().unwrap(),
                "preserve bounded owner view"
            );
            assert_eq!(scope.used.output_bytes, 0);
            assert!(scope.materialized_output_bytes > 0);
            std::fs::remove_dir_all(directory).unwrap();
        });
    }
}
