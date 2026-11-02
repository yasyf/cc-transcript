use std::path::Path;
use std::sync::Mutex;

use cc_transcript_core::ids::EventRef as RegisteredEventRef;
use cc_transcript_core::mining::{MiningError, MiningResult, MiningSignal};
use cc_transcript_core::snapshot::RegisteredReviewInput;
use pyo3::types::PyDict;

type RegisteredFrame = Arc<Mutex<Option<RegisteredReviewInput>>>;

fn format_identity(formats: &[(String, Py<PyAny>, Py<PyAny>, bool)]) -> String {
    let rows: Vec<_> = formats
        .iter()
        .map(|(name, pattern, extract, bounded)| {
            json!({"name":name,"pattern":format!("{:p}",pattern.as_ptr()),
            "extract":format!("{:p}",extract.as_ptr()),"bounded":bounded})
        })
        .collect();
    json!(rows).to_string()
}

fn run_registered_review(
    store: &NativeSnapshotStore,
    py: Python<'_>,
    path: &str,
    policy_json: &str,
    mining_spec_json: &str,
    callable_formats: Vec<(String, Py<PyAny>, Py<PyAny>, bool)>,
    first_user_marker: &str,
    prose_marker: Py<PyAny>,
    request_json: &str,
    context_json: &str,
    cancel: &SnapshotCancellation,
    handler: Py<PyAny>,
) -> PyResult<Py<PyAny>> {
    if !handler.bind(py).is_callable() || !prose_marker.bind(py).is_callable() {
        return Err(error(invalid(
            "registered review callbacks must be callable",
        )));
    }
    let formats = format_identity(&callable_formats);
    let controller =
        json!({"marker":format!("{:p}",prose_marker.as_ptr()),"formats":formats}).to_string();
    let (policy, context, limits, expected_revision, mut spec) = py
        .detach(|| {
            let policy = parse(policy_json, MAX_INPUT_BYTES)?;
            validate(
                &json!({"id":policy.get("id"),"version":policy.get("version")}),
                &SCHEMAS.classifier,
            )?;
            if let Some(classifier) = policy.get("classifier") {
                validate(classifier, &SCHEMAS.classifier)?;
            }
            let request = parse(request_json, MAX_INPUT_BYTES)?;
            let expected_revision = match request.get("source_revision") {
                None => None,
                Some(value) if value.is_null() => None,
                Some(value) => Some(
                    value
                        .as_str()
                        .ok_or_else(|| invalid("source revision must be a string"))?
                        .to_owned(),
                ),
            };
            let limits = validated(
                &json!({"limits":request.get("limits"),
            "deadline_unix_ms":request.get("deadline_unix_ms")})
                .to_string(),
                &SCHEMAS.scope_limits,
            )?;
            let context = validated(context_json, &SCHEMAS.context)?;
            if mining_spec_json.len() > MAX_INPUT_BYTES || first_user_marker.len() > MAX_INPUT_BYTES
            {
                return Err(invalid("registered review input exceeds byte limit"));
            }
            let spec = mining::compile_spec(mining_spec_json).map_err(invalid)?;
            Ok::<_, SnapshotError>((
                policy,
                context,
                scope_limits(&limits),
                expected_revision,
                spec,
            ))
        })
        .map_err(error)?;
    mining::attach_bounded_callable_formats(&mut spec, callable_formats)?;
    let owner = Arc::clone(&store.inner);
    let registry = py
        .detach(|| owner.registry_for_registered(&context))
        .map_err(error)?;
    let marker = |entries: &Arc<cc_transcript_core::snapshot::ChunkRows>| {
        Python::attach(|py| {
            with_registry(Arc::clone(&registry), || {
                let event = event_view(py, entries, 0)?;
                prose_marker.bind(py).call1((event,))?.is_truthy()
            })
        })
        .map_err(|failure| MiningError::Callback(Box::new(failure)))
    };
    let read = py.detach(|| {
        owner.with_registered_review_frame(
            Path::new(path), &context, &policy, &spec, first_user_marker, limits,
            &cancel.inner, &controller, expected_revision.as_deref(), &marker, |frame| {
                Python::attach(|py| {
                    let (preparation, handler_limits, description) = {
                        let held = frame.lock().unwrap();
                        let input = held.as_ref().expect("active registered frame");
                        (input.preparation_usage.clone(), input.handler_limits, json!({
                            "source_revision":input.source_revision(),"session_id":input.session_id(),
                            "canonical_path":input.canonical_path().to_string_lossy().as_ref(),"mtime_ns":input.checkpoint.stamp.mtime_ns.to_string(),
                            "event_count":input.entries.len(),"classifier":policy.get("classifier")
                        }))
                    };
                    let scope = Py::new(py, NativeRegisteredReviewScope {
                        owner: Arc::clone(&owner), context: context.clone(), frame,
                        registry: Some(Arc::clone(&registry)), limits:handler_limits,
                        cancel:cancel.inner.clone(), policy:policy.clone(), description,
                        spec_json:mining_spec_json.to_owned(), formats:formats.clone(),
                        preparation, used:ScopeWork::default(), materialized_output_bytes:0,
                        started:Instant::now(), failures:0, cancellations:0, closed:false,
                    })?;
                    let result = handler.bind(py).call1((scope.clone_ref(py),)).map(Bound::unbind);
                    scope.borrow_mut(py).close();
                    result
                }).map_err(|failure| MiningError::Callback(Box::new(failure)))
            },
        )
    }).map_err(mining::error)?;
    match read.value {
        Some(value) => Ok(value),
        None => {
            let result = PyDict::new(py);
            result.set_item("kind", "review_preparation")?;
            result.set_item(
                "usage",
                crate::views::convert::json_to_py(py, &read.preparation_usage)?,
            )?;
            Ok(result.into_any().unbind())
        }
    }
}

#[pyo3_stub_gen::derive::gen_stub_pyclass]
#[pyclass(name = "NativeRegisteredReviewScope", module = "cc_transcript._native")]
pub(crate) struct NativeRegisteredReviewScope {
    owner: Arc<NativeStore>,
    context: Value,
    frame: RegisteredFrame,
    registry: Option<Arc<ToolRegistrySnapshot>>,
    policy: Value,
    description: Value,
    limits: WorkLimits,
    cancel: Cancellation,
    spec_json: String,
    formats: String,
    preparation: Value,
    used: ScopeWork,
    materialized_output_bytes: usize,
    started: Instant,
    failures: usize,
    cancellations: usize,
    closed: bool,
}

impl NativeRegisteredReviewScope {
    fn read<T>(
        &self,
        consume: impl FnOnce(&RegisteredReviewInput) -> MiningResult<T>,
    ) -> MiningResult<T> {
        self.cancel.check(self.limits.deadline_unix_ms)?;
        if self.closed {
            return Err(SnapshotError::new(
                Status::StaleHandle,
                "registered review scope is closed",
            )
            .into());
        }
        let frame = self.frame.lock().unwrap();
        let input = frame.as_ref().ok_or_else(|| {
            SnapshotError::new(Status::StaleHandle, "registered review frame is closed")
        })?;
        let value = consume(input)?;
        self.cancel.check(self.limits.deadline_unix_ms)?;
        Ok(value)
    }

    fn remaining(&self) -> WorkLimits {
        let mut limits = self.used.remaining(self.limits);
        limits.max_output_bytes = limits.max_output_bytes.min(
            self.limits
                .max_output_bytes
                .saturating_sub(self.materialized_output_bytes),
        );
        limits
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

    fn mining_failure(&mut self, failure: MiningError) -> PyErr {
        match failure {
            MiningError::Snapshot(failure) => self.failure(failure),
            failure => {
                self.failures += 1;
                mining::error(failure)
            }
        }
    }

    fn charge(&mut self, work: ScopeWork) -> PyResult<()> {
        self.used
            .charge(self.limits, work)
            .map_err(|failure| self.failure(failure))
    }

    fn stage<T>(
        &self,
        py: Python<'_>,
        bytes: usize,
        convert: impl FnOnce() -> PyResult<T>,
    ) -> MiningResult<T> {
        let _reservation = py.detach(|| {
            self.owner
                .reserve_owned_input(&self.context, &self.cancel, bytes)
        })?;
        convert().map_err(|failure| MiningError::Callback(Box::new(failure)))
    }

    fn anchors(&self, text: &str) -> Result<Vec<RegisteredEventRef>, SnapshotError> {
        if text.len() > self.remaining().max_read_bytes.min(MAX_INPUT_BYTES) {
            return Err(SnapshotError::new(
                Status::SourceLimit,
                "anchor input exceeds scope budget",
            ));
        }
        let anchors = validated(text, &SCHEMAS.anchors)?;
        Ok(anchors
            .as_array()
            .expect("validated anchors")
            .iter()
            .map(|anchor| RegisteredEventRef {
                session_id: anchor
                    .get("session_id")
                    .and_then(Value::as_str)
                    .expect("validated session")
                    .to_owned(),
                event_uuid: anchor
                    .get("event_uuid")
                    .and_then(Value::as_str)
                    .expect("validated UUID")
                    .to_owned(),
                tool_use_id: anchor
                    .get("tool_use_id")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            })
            .collect())
    }
}

#[pyo3_stub_gen::derive::gen_stub_pymethods]
#[pymethods]
impl NativeRegisteredReviewScope {
    fn close(&mut self) {
        self.closed = true;
        self.registry = None;
        self.spec_json.clear();
        self.formats.clear();
    }

    fn checkpoint(&mut self, py: Python<'_>) -> PyResult<()> {
        py.detach(|| self.read(|_| Ok(())))
            .map_err(|failure| self.mining_failure(failure))
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
        let mut value = self.preparation.clone();
        for (key, added) in [
            ("requests_failed", self.failures),
            ("requests_cancelled", self.cancellations),
            ("output_bytes", self.used.output_bytes),
        ] {
            let prior = value
                .get(key)
                .and_then(Value::as_u64)
                .expect("native usage counter");
            value.insert(key, json!(prior.saturating_add(added as u64)));
        }
        value.to_string()
    }

    fn work(&self) -> String {
        json!({"read_bytes":self.used.read_bytes,"events":self.used.events,"items":self.used.items,
            "output_bytes":self.used.output_bytes,"materialized_output_bytes":self.materialized_output_bytes,
            "elapsed_ms":self.started.elapsed().as_millis() as u64}).to_string()
    }

    fn description(&mut self, py: Python<'_>) -> PyResult<String> {
        self.checkpoint(py)?;
        let encoded = snapshot_codec::encode(&self.description, self.remaining().max_output_bytes)
            .map_err(|failure| self.failure(failure))?;
        self.materialized_output_bytes += encoded.len();
        Ok(encoded)
    }

    fn event_count(&mut self, py: Python<'_>) -> PyResult<usize> {
        py.detach(|| self.read(|input| Ok(input.entries.len())))
            .map_err(|failure| self.mining_failure(failure))
    }

    fn event<'py>(&mut self, py: Python<'py>, index: usize) -> PyResult<Bound<'py, PyAny>> {
        if index >= self.event_count(py)? {
            return Err(PyIndexError::new_err("event index out of range"));
        }
        let limits = self.remaining();
        let (entry, bytes) = py
            .detach(|| {
                self.read(|input| {
                    let entry = input.entries.get(index).expect("bounded registered event");
                    let bytes = snapshot_codec::encoded_size_bounded(
                        &snapshot_codec::EventWire::new(index, &entry[0]),
                        snapshot_codec::Bound::record("registered event", limits.max_output_bytes),
                    )?;
                    Ok((Arc::clone(entry), bytes))
                })
            })
            .map_err(|failure| self.mining_failure(failure))?;
        self.charge(ScopeWork {
            read_bytes: bytes,
            events: 1,
            items: 1,
            ..ScopeWork::default()
        })?;
        let registry = Arc::clone(self.registry.as_ref().expect("open registry"));
        let result = self
            .stage(py, bytes.saturating_mul(4).saturating_add(8192), || {
                with_registry(registry, || event_view(py, &entry, 0))
            })
            .map_err(|failure| self.mining_failure(failure))?;
        self.materialized_output_bytes += bytes;
        self.checkpoint(py)?;
        Ok(result)
    }

    fn source_facts(&mut self, py: Python<'_>, first_user_contains: &str) -> PyResult<String> {
        let limit = self.remaining().max_output_bytes;
        let result = py
            .detach(|| {
                self.read(|input| {
                    if input.first_user_token() != first_user_contains {
                        return Err(invalid(
                            "source fact token differs from registered controller",
                        )
                        .into());
                    }
                    let text = input.facts_json();
                    if text.len() > limit {
                        return Err(SnapshotError::new(
                            Status::OutputLimit,
                            "registered source facts exceed output budget",
                        )
                        .into());
                    }
                    Ok(text)
                })
            })
            .map_err(|failure| self.mining_failure(failure))?;
        self.charge(ScopeWork {
            items: 1,
            ..ScopeWork::default()
        })?;
        self.materialized_output_bytes += result.len();
        Ok(result)
    }

    fn mine<'py>(
        &mut self,
        py: Python<'py>,
        spec_json: &str,
        callable_formats: Vec<(String, Py<PyAny>, Py<PyAny>, bool)>,
    ) -> PyResult<Vec<Bound<'py, PyDict>>> {
        self.checkpoint(py)?;
        if spec_json != self.spec_json || format_identity(&callable_formats) != self.formats {
            return Err(self.failure(invalid(
                "mining specification differs from registered controller",
            )));
        }
        let limits = self.remaining();
        let owner = Arc::clone(&self.owner);
        let _reservation = py
            .detach(|| {
                owner.reserve_owned_input(
                    &self.context,
                    &self.cancel,
                    limits
                        .max_output_bytes
                        .saturating_mul(4)
                        .saturating_add(
                            limits
                                .max_items
                                .saturating_mul(std::mem::size_of::<&MiningSignal>()),
                        )
                        .saturating_add(8192),
                )
            })
            .map_err(|failure| self.failure(failure))?;
        let (encoded, count) = py
            .detach(|| {
                self.read(|input| {
                    let signals: Vec<_> = input
                        .mining
                        .signals()?
                        .take(limits.max_items.saturating_add(1))
                        .collect();
                    if signals.len() > limits.max_items {
                        return Err(SnapshotError::new(Status::Incomplete, "item_limit").into());
                    }
                    let text = snapshot_codec::encode_bounded(
                        &signals,
                        snapshot_codec::Bound::budget(
                            "registered signals",
                            limits.max_output_bytes,
                        ),
                    )?;
                    Ok((text, signals.len()))
                })
            })
            .map_err(|failure| self.mining_failure(failure))?;
        self.charge(ScopeWork {
            read_bytes: encoded.len(),
            events: count,
            items: count,
            ..ScopeWork::default()
        })?;
        let registry = Arc::clone(self.registry.as_ref().expect("open registry"));
        let result = self
            .stage(
                py,
                encoded.len().saturating_mul(4).saturating_add(8192),
                || {
                    let signals: Vec<MiningSignal> =
                        sonic_rs::from_str(&encoded).expect("encoded registered signals");
                    with_registry(registry, || {
                        signals
                            .iter()
                            .map(|signal| mining::build_signal_dict(py, signal))
                            .collect()
                    })
                },
            )
            .map_err(|failure| self.mining_failure(failure))?;
        self.materialized_output_bytes += encoded.len();
        self.checkpoint(py)?;
        Ok(result)
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
        if preview_chars != 200 {
            return Err(self.failure(invalid(
                "registered context preview is fixed at 200 characters",
            )));
        }
        let limits = self.remaining();
        let anchors = self
            .anchors(anchors_json)
            .map_err(|failure| self.failure(failure))?;
        if anchors.len() > limits.max_items {
            return Err(self.failure(SnapshotError::new(Status::Incomplete, "item_limit")));
        }
        let owner = Arc::clone(&self.owner);
        let _reservation = py
            .detach(|| {
                owner.reserve_owned_input(
                    &self.context,
                    &self.cancel,
                    limits
                        .max_output_bytes
                        .saturating_mul(4)
                        .saturating_add(8192),
                )
            })
            .map_err(|failure| self.failure(failure))?;
        let (result, events) = py
            .detach(|| {
                self.read(|input| {
                    let mut remaining = limits.max_output_bytes;
                    let mut events = 0usize;
                    let mut windows = Vec::with_capacity(anchors.len());
                    for anchor in &anchors {
                        self.cancel.check(limits.deadline_unix_ms)?;
                        let turn = input
                            .activity
                            .turn_of_uuid(&anchor.event_uuid)
                            .ok_or_else(|| invalid("context anchor not found"))?;
                        let selected = turn
                            .saturating_add(after)
                            .saturating_add(1)
                            .min(input.activity.turn_count())
                            - turn.saturating_sub(before);
                        if selected > limits.max_events.saturating_sub(events) {
                            return Err(
                                SnapshotError::new(Status::Incomplete, "event_limit").into()
                            );
                        }
                        let window =
                            input
                                .contexts
                                .capture(&input.activity, anchor, before, after)?;
                        let examined = window
                            .before
                            .iter()
                            .chain(window.trigger.as_ref())
                            .chain(&window.after)
                            .map(|turn| turn.refs.len())
                            .sum::<usize>();
                        if examined > limits.max_events.saturating_sub(events) {
                            return Err(
                                SnapshotError::new(Status::Incomplete, "event_limit").into()
                            );
                        }
                        events += examined;
                        let encoded = window.to_json_bounded(remaining).map_err(|failure| {
                            SnapshotError::new(Status::OutputLimit, format!("{failure:?}"))
                        })?;
                        remaining = remaining.saturating_sub(encoded.len());
                        windows.push(encoded);
                    }
                    Ok((
                        snapshot_codec::encode(&json!(windows), limits.max_output_bytes)?,
                        events,
                    ))
                })
            })
            .map_err(|failure| self.mining_failure(failure))?;
        self.charge(ScopeWork {
            read_bytes: anchors_json.len(),
            events,
            items: anchors.len(),
            ..ScopeWork::default()
        })?;
        self.materialized_output_bytes += result.len();
        Ok(result)
    }

    #[pyo3(signature = (classifier_json, anchors_json=None, lookback=40, lookahead=120))]
    fn activity<'py>(
        &mut self,
        py: Python<'py>,
        classifier_json: &str,
        anchors_json: Option<&str>,
        lookback: usize,
        lookahead: usize,
    ) -> PyResult<Bound<'py, PyDict>> {
        self.checkpoint(py)?;
        let classifier = validated(classifier_json, &SCHEMAS.classifier)
            .map_err(|failure| self.failure(failure))?;
        if self.policy.get("classifier") != Some(&classifier) {
            return Err(self.failure(invalid("classifier differs from registered controller")));
        }
        let anchors = anchors_json
            .map(|text| self.anchors(text))
            .transpose()
            .map_err(|failure| self.failure(failure))?;
        let limits = self.remaining();
        let owner = Arc::clone(&self.owner);
        let _reservation = py
            .detach(|| {
                owner.reserve_owned_input(
                    &self.context,
                    &self.cancel,
                    limits
                        .max_output_bytes
                        .saturating_mul(4)
                        .saturating_add(8192),
                )
            })
            .map_err(|failure| self.failure(failure))?;
        let (records, session_id, bytes, events) = py
            .detach(|| {
                self.read(|input| {
                    let range = if let Some(anchors) = &anchors {
                        let mut selected = None::<(usize, usize)>;
                        for anchor in anchors {
                            if anchor.session_id != input.session_id() {
                                return Err(invalid("activity anchor session mismatch").into());
                            }
                            let turn = input
                                .activity
                                .turn_of_uuid(&anchor.event_uuid)
                                .ok_or_else(|| invalid("activity anchor not found"))?;
                            selected = Some(selected.map_or((turn, turn), |(first, last)| {
                                (first.min(turn), last.max(turn))
                            }));
                        }
                        selected.map_or(0..0, |(first, last)| {
                            first.saturating_sub(lookback)
                                ..last
                                    .saturating_add(lookahead)
                                    .saturating_add(1)
                                    .min(input.activity.turn_count())
                        })
                    } else {
                        0..input.activity.turn_count()
                    };
                    if range.len() > limits.max_items {
                        return Err(SnapshotError::new(Status::Incomplete, "item_limit").into());
                    }
                    let mut examined = 0usize;
                    for turn in range.clone() {
                        self.cancel.check(limits.deadline_unix_ms)?;
                        let bounds = input.activity.turn_bounds(turn).expect("registered turn");
                        examined = examined
                            .saturating_add(bounds.len())
                            .saturating_add(input.activity.result_events(turn).count());
                        if examined > limits.max_events {
                            return Err(
                                SnapshotError::new(Status::Incomplete, "event_limit").into()
                            );
                        }
                    }
                    let turns = input.activity_turns(range, &limits, &self.cancel)?;
                    let events = turns.iter().map(|turn| turn.events.len()).sum();
                    let wires: Vec<_> = turns
                        .iter()
                        .map(|turn| {
                            let start = input
                                .activity
                                .turn_bounds(turn.index)
                                .expect("registered turn")
                                .start;
                            snapshot_codec::TurnWire {
                                codec: snapshot_codec::TURN_CODEC,
                                index: turn.index,
                                prompt: &turn.prompt,
                                started_at: turn.started_at,
                                ended_at: turn.ended_at,
                                events: turn
                                    .events
                                    .iter()
                                    .enumerate()
                                    .map(|(offset, event)| {
                                        snapshot_codec::EventWire::new(start + offset, event)
                                    })
                                    .collect(),
                                tool_uses: turn
                                    .tool_uses
                                    .iter()
                                    .map(|tool| {
                                        snapshot_codec::ToolUseWire::new(tool, input.session_id())
                                    })
                                    .collect(),
                            }
                        })
                        .collect();
                    let bound = snapshot_codec::Bound::budget(
                        "registered activity",
                        limits.max_output_bytes,
                    );
                    let bytes = snapshot_codec::encoded_size_bounded(
                        &snapshot_codec::ActivityWire {
                            session_id: input.session_id(),
                            turns: &wires,
                        },
                        bound,
                    )?;
                    let encoded = wires
                        .iter()
                        .map(|wire| snapshot_codec::encode_bounded(wire, bound))
                        .collect::<Result<Vec<_>, _>>()?;
                    Ok((
                        snapshot_codec::decode_owned_turns(&encoded)?,
                        input.session_id().to_owned(),
                        bytes,
                        events,
                    ))
                })
            })
            .map_err(|failure| self.mining_failure(failure))?;
        self.charge(ScopeWork {
            read_bytes: bytes,
            events,
            items: records.len(),
            ..ScopeWork::default()
        })?;
        let registry = Arc::clone(self.registry.as_ref().expect("open registry"));
        let payload = self
            .stage(py, bytes.saturating_mul(4).saturating_add(8192), || {
                with_registry(registry, || {
                    let turns = crate::snapshot_decode::detached_turns(py, records)?;
                    let payload = PyDict::new(py);
                    payload.set_item("session_id", session_id)?;
                    payload.set_item("turns", PyTuple::new(py, turns)?)?;
                    Ok(payload)
                })
            })
            .map_err(|failure| self.mining_failure(failure))?;
        self.materialized_output_bytes += bytes;
        self.checkpoint(py)?;
        Ok(payload)
    }

    fn hook_marker_indices(&mut self, py: Python<'_>) -> PyResult<Vec<usize>> {
        let limits = self.remaining();
        let rows = py
            .detach(|| {
                self.read(|input| {
                    let rows = input.hook_marker_indices();
                    if rows.len() > limits.max_items {
                        return Err(SnapshotError::new(Status::Incomplete, "item_limit").into());
                    }
                    snapshot_codec::encoded_size(&json!(rows), limits.max_output_bytes)?;
                    Ok(rows.to_vec())
                })
            })
            .map_err(|failure| self.mining_failure(failure))?;
        self.charge(ScopeWork {
            items: rows.len(),
            ..ScopeWork::default()
        })?;
        Ok(rows)
    }

    fn tool_use_digests(&mut self, py: Python<'_>) -> PyResult<String> {
        let limits = self.remaining();
        let (result, items) = py
            .detach(|| {
                self.read(|input| {
                    let digests = input.tool_use_digests();
                    if digests.len() > limits.max_items {
                        return Err(SnapshotError::new(Status::Incomplete, "item_limit").into());
                    }
                    Ok((
                        snapshot_codec::encode(&json!(digests), limits.max_output_bytes)?,
                        digests.len(),
                    ))
                })
            })
            .map_err(|failure| self.mining_failure(failure))?;
        self.charge(ScopeWork {
            items,
            ..ScopeWork::default()
        })?;
        self.materialized_output_bytes += result.len();
        Ok(result)
    }

    fn publish_projection<'py>(
        &mut self,
        py: Python<'py>,
        request_json: &str,
        metadata_json: &str,
        field: &str,
        records_json: &Bound<'py, PyList>,
        usage_json: &str,
        work_json: &str,
    ) -> PyResult<String> {
        use cc_transcript_core::snapshot_owned::{MAX_OWNED_BYTES, MAX_OWNED_RECORDS};
        self.checkpoint(py)?;
        let length = records_json.len();
        if length > MAX_OWNED_RECORDS {
            return Err(self.failure(SnapshotError::new(
                Status::OutputLimit,
                "too many owned records",
            )));
        }
        let remaining = self.remaining();
        let (mut request, metadata, usage, work) = py.detach(|| {
            let request = parse(request_json, MAX_INPUT_BYTES)?;
            validated(&json!({"limits":request.get("limits"),"deadline_unix_ms":request.get("deadline_unix_ms")}).to_string(), &SCHEMAS.scope_limits)?;
            if request.get("id").and_then(Value::as_str).is_none() {
                return Err(invalid("registered publication requires an id"));
            }
            let metadata = parse(metadata_json, MAX_INPUT_BYTES)?;
            if !metadata.is_object() {
                return Err(invalid("registered publication metadata must be an object"));
            }
            Ok::<_, SnapshotError>((request,metadata,validated(usage_json,&SCHEMAS.usage)?,validated(work_json,&SCHEMAS.work)?))
        }).map_err(|failure| self.failure(failure))?;
        request.insert("limits", self.limits.to_json());
        request.insert("deadline_unix_ms", json!(remaining.deadline_unix_ms));
        let owner = Arc::clone(&self.owner);
        let pointer_bytes = length.saturating_mul(
            std::mem::size_of::<Bound<'_, PyString>>() + std::mem::size_of::<&str>(),
        );
        let _pointers = py
            .detach(|| owner.reserve_owned_input(&self.context, &self.cancel, pointer_bytes))
            .map_err(|failure| self.failure(failure))?;
        if records_json.len() > length {
            return Err(self.failure(invalid("record list grew during admission")));
        }
        let mut strings = Vec::with_capacity(length);
        let mut characters = 0usize;
        for record in records_json.iter() {
            let record = record.cast_into_exact::<PyString>()?;
            characters = characters.checked_add(record.len()?).ok_or_else(|| {
                self.failure(SnapshotError::new(
                    Status::OutputLimit,
                    "owned record length overflow",
                ))
            })?;
            if characters > MAX_OWNED_BYTES {
                return Err(self.failure(SnapshotError::new(
                    Status::OutputLimit,
                    "owned records exceed byte limit",
                )));
            }
            strings.push(record);
        }
        let _utf8 = py
            .detach(|| {
                owner.reserve_owned_input(&self.context, &self.cancel, characters.saturating_mul(4))
            })
            .map_err(|failure| self.failure(failure))?;
        self.checkpoint(py)?;
        let records = strings
            .iter()
            .map(|record| record.to_str())
            .collect::<PyResult<Vec<_>>>()?;
        let response = py
            .detach(|| {
                self.read(|input| {
                    let revision = input.source_revision();
                    if metadata
                        .get("source_revision")
                        .is_some_and(|value| value.as_str() != Some(revision.as_str()))
                    {
                        return Err(invalid(
                            "publication source revision differs from registered frame",
                        )
                        .into());
                    }
                    let mut metadata = metadata.clone();
                    metadata.insert("source_revision", json!(revision));
                    let response = self.owner.publish_registered_projection(
                        input,
                        &request,
                        &self.context,
                        &self.cancel,
                        &metadata,
                        field,
                        &records,
                        &usage,
                        &work,
                    )?;
                    Ok(response.json().to_owned())
                })
            })
            .map_err(|failure| self.mining_failure(failure))?;
        if response.len() > remaining.max_output_bytes {
            return Err(self.failure(SnapshotError::new(
                Status::OutputLimit,
                "registered publication reply exceeds output budget",
            )));
        }
        self.materialized_output_bytes += response.len();
        self.checkpoint(py)?;
        Ok(response)
    }
}
