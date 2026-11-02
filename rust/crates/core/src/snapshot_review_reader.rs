use crate::snapshot_ledger::{
    deque_reallocation_bound, map_reallocation_bound, set_reallocation_bound,
    vec_reallocation_bound,
};

pub struct RegisteredReviewInput {
    pub checkpoint: RegisteredReviewCheckpoint,
    pub mining: crate::mining::ReviewMiningIndex,
    pub facts: crate::snapshot_text::SourceFactsIndex,
    pub activity: ActivityIndex,
    pub entries: Vec<Arc<ChunkRows>>,
    pub handler_limits: WorkLimits,
    pub preparation_progress: crate::scan::ScanProgress,
    pub preparation_usage: Value,
    preparation_decoded: usize,
    preparation_lifts: usize,
    codex: Option<crate::codex::CodexReviewContinuation>,
    provider: Option<Provider>,
    raw_spans: Vec<crate::scan_stream::LineSpan>,
    pending_rows: VecDeque<(Entry, crate::scan_stream::LineSpan)>,
    pending_row_bytes: usize,
    pending_codex: Range<usize>,
    canonical_path: PathBuf,
    session_id: String,
    meta_events: Vec<usize>,
    model_events: Vec<usize>,
    retained_entries: usize,
    pending_line: Vec<u8>,
    scan_complete: bool,
    first_user_token: String,
    hook_markers: Vec<usize>,
    digests: HashMap<String, String>,
    digest_bytes: usize,
    pub contexts: crate::snapshot_review_context::ReviewContextState,
    spans: Vec<crate::scan_stream::LineSpan>,
    touched: u64,
}

impl RegisteredReviewInput {
    pub fn accounted_bytes(&self) -> usize {
        size_of::<Self>() + self.checkpoint.accounted_bytes()
            - size_of::<RegisteredReviewCheckpoint>()
            + self.mining.accounted_bytes()
            - size_of::<crate::mining::ReviewMiningIndex>()
            + self.facts.accounted_bytes()
            - size_of::<crate::snapshot_text::SourceFactsIndex>()
            + self.activity.accounted_bytes()
            - size_of::<ActivityIndex>()
            + self.contexts.accounted_bytes()
            - size_of::<crate::snapshot_review_context::ReviewContextState>()
            + self.raw_spans.capacity() * size_of::<crate::scan_stream::LineSpan>()
            + self.pending_rows.capacity() * size_of::<(Entry, crate::scan_stream::LineSpan)>()
            + self.pending_row_bytes
            + self.codex.as_ref().map_or(0, |codex| {
                codex.accounted_bytes() - size_of::<crate::codex::CodexReviewContinuation>()
            })
            + crate::snapshot_memory::value_charge(&self.preparation_usage)
                .opaque_dom_accounted_bytes
            + self.entries.capacity() * size_of::<Arc<ChunkRows>>()
            + self.canonical_path.as_os_str().len()
            + self.session_id.capacity()
            + (self.meta_events.capacity() + self.model_events.capacity()) * size_of::<usize>()
            + self.retained_entries
            + self.pending_line.capacity()
            + self.first_user_token.capacity()
            + self.hook_markers.capacity() * size_of::<usize>()
            + self.digests.capacity() * size_of::<(String, String)>()
            + self.digest_bytes
            + self.spans.capacity() * size_of::<crate::scan_stream::LineSpan>()
    }

    pub(crate) fn publication_source(&self) -> Result<&Path, SnapshotError> {
        if !self.scan_complete {
            return Err(SnapshotError::new(
                Status::Incomplete,
                "registered source is not prepared",
            ));
        }
        Ok(&self.canonical_path)
    }

    pub fn canonical_path(&self) -> &Path {
        &self.canonical_path
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub fn activity_turns(
        &self,
        turns: Range<usize>,
        limits: &WorkLimits,
        cancel: &Cancellation,
    ) -> Result<Vec<crate::activity::Turn<'_>>, SnapshotError> {
        cancel.check(limits.deadline_unix_ms)?;
        if turns.len() > limits.max_events {
            return Err(SnapshotError::new(
                Status::Incomplete,
                "activity event limit",
            ));
        }
        Ok(turns
            .map(|index| {
                self.activity
                    .project_turn_with(index, |event| &self.entries[event][0])
                    .expect("bounded activity turn")
            })
            .collect())
    }

    pub fn source_revision(&self) -> String {
        let stamp = self.checkpoint.stamp;
        format!(
            "review:{}:{}:{}:{}:{}:{:02x?}",
            stamp.identity.device,
            stamp.identity.inode,
            stamp.size,
            stamp.mtime_ns,
            stamp.ctime_ns,
            self.checkpoint.epoch
        )
    }

    pub fn first_user_token(&self) -> &str {
        &self.first_user_token
    }
    pub fn hook_marker_indices(&self) -> &[usize] {
        &self.hook_markers
    }
    pub fn tool_use_digests(&self) -> &HashMap<String, String> {
        &self.digests
    }
    pub fn facts_json(&self) -> String {
        sonic_rs::to_string(self.facts.facts()).expect("source facts")
    }

    pub fn event_span(&self, event: usize) -> Option<crate::scan_stream::LineSpan> {
        self.spans.get(event).copied()
    }
}

impl Charge<[u8; 32]> for RegisteredReviewInput {
    fn charge(&self) -> usize {
        self.accounted_bytes() - size_of::<Self>()
    }
}

pub struct RegisteredReviewRead<T> {
    pub value: T,
    pub progress: crate::scan::ScanProgress,
    pub mining_usage: crate::mining::MiningUsage,
    pub complete: bool,
    pub preparation_usage: Value,
}

struct RegisteredReviewPending<'store, 'context> {
    store: &'store NativeStore,
    canonical: PathBuf,
    context: &'context Value,
    input: Option<RegisteredReviewInput>,
    stream: crate::scan_stream::SourceStream<'store>,
    budget: crate::scan::ScanBudget<'store>,
    reservation: ProjectionReservation<'store>,
    mining_usage: crate::mining::MiningUsage,
    _claim: FactReadClaim<'store>,
}

impl RegisteredReviewPending<'_, '_> {
    fn finish<T>(mut self, value: T) -> crate::mining::MiningResult<RegisteredReviewRead<T>> {
        self.stream.verify()?;
        let input = self
            .input
            .take()
            .expect("registered input returned by callback");
        let complete = input.scan_complete;
        let preparation_usage = input.preparation_usage.clone();
        self.store.authority(self.context, Some(&self.canonical))?;
        self.store.registry(self.context)?;
        self.store
            .publish_review_input(input, self.context, &mut self.reservation)?;
        Ok(RegisteredReviewRead {
            value,
            progress: self.budget.progress,
            mining_usage: self.mining_usage,
            complete,
            preparation_usage,
        })
    }
}

impl NativeStore {
    pub(crate) fn registered_projection_expiry(
        &self,
        path: &Path,
        context: &Value,
        deadline: u64,
    ) -> Result<u64, SnapshotError> {
        self.authority(context, Some(path))?;
        self.registry(context)?;
        self.owned_handle_expiry(&[], context, deadline)
    }

    pub fn registry_for_registered(
        &self,
        context: &Value,
    ) -> Result<Arc<crate::toolcall::ToolRegistrySnapshot>, SnapshotError> {
        self.registry(context)
    }

    fn review_reserve(
        &self,
        reservation: &mut ProjectionReservation<'_>,
        context: &Value,
        needed: usize,
    ) -> Result<(), SnapshotError> {
        if needed > reservation.bytes {
            self.extend_projection_reservation(reservation, context, needed - reservation.bytes)?;
        }
        Ok(())
    }

    fn review_settle(&self, reservation: &mut ProjectionReservation<'_>, bytes: usize) {
        assert!(
            bytes <= reservation.bytes,
            "review input exceeded admitted storage"
        );
        let released = reservation.bytes - bytes;
        self.lock_state().transient_bytes -= released;
        reservation.bytes = bytes;
    }

    fn take_review_input(
        &self,
        epoch: &[u8; 32],
    ) -> (Option<RegisteredReviewInput>, ProjectionReservation<'_>) {
        let mut state = self.lock_state();
        let input = state.review_inputs.remove(epoch);
        let bytes = input
            .as_ref()
            .map_or(0, RegisteredReviewInput::accounted_bytes);
        if let Some(input) = &input {
            assert!(
                state
                    .cache_order
                    .remove(&(input.touched, CacheOwner::Review(*epoch)))
            );
        }
        state.transient_bytes += bytes;
        (input, ProjectionReservation { store: self, bytes })
    }

    fn publish_review_input(
        &self,
        mut input: RegisteredReviewInput,
        context: &Value,
        reservation: &mut ProjectionReservation<'_>,
    ) -> Result<(), SnapshotError> {
        let key = input.checkpoint.epoch;
        let mut state = self.lock_state();
        let growth = state.review_inputs.growth_for(&key) + size_of::<(u64, CacheOwner)>();
        let additional = input.charge() + growth;
        let covered = additional.min(reservation.bytes);
        self.admit_memory(&mut state, context, additional - covered)?;
        state.review_inputs.reserve_for(&key);
        input.touched = state.next_cache_touch();
        assert!(
            state
                .cache_order
                .insert((input.touched, CacheOwner::Review(key)))
        );
        assert!(state.review_inputs.insert(key, input).is_none());
        state.transient_bytes -= covered;
        reservation.bytes -= covered;
        Ok(())
    }

    fn review_decode(
        &self,
        bytes: &[u8],
        context: &Value,
        reservation: &mut ProjectionReservation<'_>,
        retained: usize,
    ) -> Result<Vec<Entry>, SnapshotError> {
        if bytes.len() > self.config.entry {
            return Err(SnapshotError::new(
                Status::EntryLimit,
                "source entry exceeds owner bound",
            ));
        }
        let overflow = || invalid("review entry allocation bound overflow");
        let dom_bound = dom_parse_bound(bytes.len()).ok_or_else(overflow)?;
        self.review_reserve(
            reservation,
            context,
            retained.checked_add(dom_bound).ok_or_else(overflow)?,
        )?;
        self.before_decode("registered-review-entry");
        let Some(dom) = crate::parse::parse_line_dom(bytes) else {
            return Ok(Vec::new());
        };
        let typed = crate::parse::retained_entry_bound(&dom).ok_or_else(overflow)?
            + crate::parse::retained_arena_bound(&dom)
            + size_of::<Entry>();
        self.review_reserve(
            reservation,
            context,
            retained
                .checked_add(dom_bound)
                .and_then(|bytes| bytes.checked_add(typed))
                .ok_or_else(overflow)?,
        )?;
        let entry =
            crate::parse::entry_from_dom(dom, &mut crate::parse::Retained::default(), &|_| true)
                .map_err(|error| SnapshotError::new(Status::ParseError, format!("{error:?}")))?;
        Ok(entry.into_iter().collect())
    }

    pub fn with_registered_claude_review<'store, T>(
        &'store self,
        path: &Path,
        context: &Value,
        policy: &Value,
        spec: &crate::mining::CompiledMiningSpec,
        first_user_token: &str,
        limits: WorkLimits,
        cancel: &Cancellation,
        consume: impl FnOnce(
            &RegisteredReviewInput,
            &mut crate::scan_stream::SourceStream<'store>,
            &mut crate::scan::ScanBudget<'store>,
        ) -> crate::mining::MiningResult<T>,
    ) -> crate::mining::MiningResult<RegisteredReviewRead<T>> {
        let mut pending = self.read_registered_claude_review(
            path,
            context,
            policy,
            spec,
            first_user_token,
            limits,
            cancel,
            None,
            "",
            None,
        )?;
        let value = consume(
            pending.input.as_ref().expect("registered input"),
            &mut pending.stream,
            &mut pending.budget,
        )?;
        pending.finish(value)
    }

    pub fn with_registered_review_frame<T>(
        &self,
        path: &Path,
        context: &Value,
        policy: &Value,
        spec: &crate::mining::CompiledMiningSpec,
        first_user_token: &str,
        limits: WorkLimits,
        cancel: &Cancellation,
        controller_identity: &str,
        expected_revision: Option<&str>,
        prose_marker: &dyn Fn(&Arc<ChunkRows>) -> crate::mining::MiningResult<bool>,
        consume: impl FnOnce(
            Arc<Mutex<Option<RegisteredReviewInput>>>,
        ) -> crate::mining::MiningResult<T>,
    ) -> crate::mining::MiningResult<RegisteredReviewRead<Option<T>>> {
        let mut pending = self.read_registered_claude_review(
            path,
            context,
            policy,
            spec,
            first_user_token,
            limits,
            cancel,
            Some(prose_marker),
            controller_identity,
            expected_revision,
        )?;
        if !pending
            .input
            .as_ref()
            .expect("registered input")
            .scan_complete
        {
            return pending.finish(None);
        }
        let frame = Arc::new(Mutex::new(pending.input.take()));
        let value = consume(Arc::clone(&frame));
        pending.input = frame.lock().expect("registered review frame").take();
        match value {
            Ok(value) => pending.finish(Some(value)),
            Err(error) => {
                pending.finish(())?;
                Err(error)
            }
        }
    }

    fn fold_registered_review_entry(
        &self,
        input: &mut RegisteredReviewInput,
        entry: &Entry,
        line: crate::scan_stream::LineSpan,
        context: &Value,
        spec: &crate::mining::CompiledMiningSpec,
        registry: &Arc<crate::toolcall::ToolRegistrySnapshot>,
        budget: &mut crate::scan::ScanBudget<'_>,
        cancel: &Cancellation,
        reservation: &mut ProjectionReservation<'_>,
        prose_marker: Option<&dyn Fn(&Arc<ChunkRows>) -> crate::mining::MiningResult<bool>>,
        classifier: Option<&Arc<ClassifierCallback>>,
    ) -> crate::mining::MiningResult<()> {
        budget.charge_projection(0, 1, cancel)?;
        let charge = entry_charge(&entry);
        let heap = charge.owned_capacity_bytes + charge.opaque_dom_accounted_bytes;
        let bound = input.mining.append_bound(&entry, spec)?
            + vec_reallocation_bound(&input.spans, 1)
            + vec_reallocation_bound(&input.entries, 1)
            + vec_reallocation_bound(&input.meta_events, 1)
            + vec_reallocation_bound(&input.model_events, 1)
            + vec_reallocation_bound(&input.hook_markers, 1)
            + map_reallocation_bound(&input.digests, entry.tool_uses().count())
            + input.activity.append_key_reservation_bytes(
                1,
                entry.tool_results().count(),
                entry
                    .meta()
                    .map_or(0, |meta| arc_slice_bytes::<u8>(meta.uuid.len())),
                entry
                    .tool_results()
                    .map(|result| arc_slice_bytes::<u8>(result.tool_use_id.len()))
                    .max()
                    .unwrap_or(0),
            )
            + input.contexts.append_bound(entry)
            + input.facts.append_bound(entry)
            + heap * 10
            + 4096
            + input.activity.append_container_reservation_bytes(
                1,
                entry.tool_uses().count(),
                entry.tool_results().count(),
            );
        self.review_reserve(
            reservation,
            context,
            input.accounted_bytes() + heap + size_of::<Entry>() + bound,
        )?;
        let refs = [entry];
        let raw = sonic_rs::to_vec(&entry).map_err(|error| invalid(error.to_string()))?;
        let copied: Entry =
            sonic_rs::from_slice(&raw).map_err(|error| invalid(error.to_string()))?;
        let rows = Arc::new(ChunkRows::new(vec![copied]));
        let marked = match prose_marker {
            Some(marker) => marker(&rows)?,
            None => false,
        };
        let projected = registered_review_entry(
            Arc::try_unwrap(rows)
                .expect("synchronous marker must not retain events")
                .rows
                .pop()
                .expect("one event"),
            marked,
        )?;
        let opener = if let Some(classifier) = classifier {
            let copied: Entry =
                sonic_rs::from_slice(&raw).map_err(|error| invalid(error.to_string()))?;
            let labels = classifier(&[Arc::new(EntryChunk::new(0, vec![copied]))], 0..1)?;
            if labels.len() != 1 {
                return Err(invalid("classifier returned wrong label count").into());
            }
            Some(labels)
        } else {
            None
        };
        let mut usage = crate::snapshot_text::TextUsage::default();
        input
            .facts
            .append(refs, &budget.remaining(), cancel, &mut usage)?;
        budget.commit_projection(usage.read_bytes, usage.events)?;
        input.mining.append(&refs, spec)?;
        let projected_refs = [&projected];
        input.activity = crate::toolcall::with_registry(Arc::clone(registry), || {
            std::mem::take(&mut input.activity).append_tail(&projected_refs, opener.as_deref())
        });
        crate::toolcall::with_registry(Arc::clone(registry), || {
            input
                .contexts
                .append(input.mining.event_count() - 1, &entry, &input.activity);
        });
        let position = input.entries.len();
        if let Some(meta) = projected.meta() {
            if input.meta_events.is_empty() {
                input.session_id.clone_from(&meta.session_id);
            }
            input.meta_events.push(position);
        }
        if matches!(&projected, Entry::Assistant(assistant) if assistant.model != crate::types::SYNTHETIC_MODEL)
        {
            input.model_events.push(position);
        }
        if marked {
            input.hook_markers.push(position);
        }
        for tool in projected.tool_uses() {
            let digest = crate::ids::tool_digest(&tool.name, &tool.input).map_err(invalid)?;
            let key = tool.id.clone();
            if let Some(old) = input.digests.get(&key) {
                input.digest_bytes -= old.capacity();
            } else {
                input.digest_bytes += key.capacity();
            }
            input.digest_bytes += digest.capacity();
            input.digests.insert(key, digest);
        }
        let charge = entry_charge(&projected);
        input.retained_entries += arc_bytes::<ChunkRows>()
            + size_of::<Entry>()
            + charge.owned_capacity_bytes
            + charge.opaque_dom_accounted_bytes;
        input
            .entries
            .push(Arc::new(ChunkRows::new(vec![projected])));
        input.spans.push(line);
        input.preparation_lifts += 1;

        Ok(())
    }

    fn truncate_registered_review_input(
        &self,
        input: &mut RegisteredReviewInput,
        first: usize,
        spec: &crate::mining::CompiledMiningSpec,
        context: &Value,
        reservation: &mut ProjectionReservation<'_>,
    ) -> crate::mining::MiningResult<()> {
        let mut ids = HashSet::new();
        let tools = input.entries[first..]
            .iter()
            .map(|rows| rows[0].tool_uses().count())
            .sum::<usize>();
        let bytes = input.entries[first..]
            .iter()
            .flat_map(|rows| rows[0].tool_uses())
            .map(|tool| tool.id.len())
            .sum::<usize>();
        self.review_reserve(
            reservation,
            context,
            input.accounted_bytes() + set_reallocation_bound(&ids, tools) + bytes + 8192,
        )?;
        for rows in &input.entries[first..] {
            for tool in rows[0].tool_uses() {
                ids.insert(tool.id.clone());
            }
        }
        self.review_reserve(
            reservation,
            context,
            input.accounted_bytes()
                + input.mining.truncate_bound(first, &ids)
                + set_reallocation_bound(&HashSet::<String>::new(), tools)
                + bytes
                + 8192,
        )?;
        while input
            .meta_events
            .last()
            .is_some_and(|event| *event >= first)
        {
            input.meta_events.pop();
        }
        while input
            .model_events
            .last()
            .is_some_and(|event| *event >= first)
        {
            input.model_events.pop();
        }
        input.mining.truncate(first, spec)?;
        input.activity.truncate(
            first,
            &input.entries,
            |id| input.mining.last_result(id),
            input.meta_events.first().copied(),
            input.meta_events.last().copied(),
            input.model_events.last().copied(),
        );
        input.contexts.truncate(first);
        input.facts.truncate(first)?;
        for rows in &input.entries[first..] {
            let charge = entry_charge(&rows[0]);
            input.retained_entries -= arc_bytes::<ChunkRows>()
                + size_of::<Entry>()
                + charge.owned_capacity_bytes
                + charge.opaque_dom_accounted_bytes;
        }
        input.entries.truncate(first);
        input.spans.truncate(first);
        input
            .hook_markers
            .truncate(input.hook_markers.partition_point(|event| *event < first));
        if input.meta_events.is_empty() {
            input.session_id.clear();
        }
        for id in ids {
            if let Some((key, value)) = input.digests.remove_entry(&id) {
                input.digest_bytes -= key.capacity() + value.capacity();
            }
            if let Some(event) = input.mining.tool_position(&id) {
                let tool = input.entries[event][0]
                    .tool_uses()
                    .filter(|tool| tool.id == id)
                    .last()
                    .expect("retained tool use");
                let digest = crate::ids::tool_digest(&tool.name, &tool.input)
                    .expect("previously indexed digest");
                input.digest_bytes += id.capacity() + digest.capacity();
                input.digests.insert(id, digest);
            }
        }
        input.checkpoint.events = first;
        Ok(())
    }

    fn read_registered_claude_review<'store, 'context>(
        &'store self,
        path: &Path,
        context: &'context Value,
        policy: &Value,
        spec: &crate::mining::CompiledMiningSpec,
        first_user_token: &str,
        limits: WorkLimits,
        cancel: &Cancellation,
        prose_marker: Option<&dyn Fn(&Arc<ChunkRows>) -> crate::mining::MiningResult<bool>>,
        controller_identity: &str,
        expected_revision: Option<&str>,
    ) -> crate::mining::MiningResult<RegisteredReviewPending<'store, 'context>> {
        use crate::scan_stream::{LineSpan, SourceStream};
        cancel.check(limits.deadline_unix_ms)?;
        let canonical = std::fs::canonicalize(path).map_err(io_error)?;
        self.authority(context, Some(&canonical))?;
        let registry = self.registry(context)?;
        let binding = json!({
            "policy":{"id":str_field(policy,"id")?,"version":str_field(policy,"version")?},
            "spec_fingerprint":spec.fingerprint(),"provider":"registered-agent","first_user_token":first_user_token,"controller_identity":controller_identity,
        });
        let classifier = policy
            .get("classifier")
            .map(|classifier| {
                let id = str_field(classifier, "id")?;
                let version = str_field(classifier, "version")?;
                if id == "native" && version == "1" {
                    return Ok(None);
                }
                let key = sonic_rs::to_string(&json!([id, version])).expect("classifier key");
                self.classifiers
                    .lock()
                    .expect("classifiers")
                    .get(&key)
                    .cloned()
                    .map(Some)
                    .ok_or_else(|| invalid("registered review classifier is unavailable"))
            })
            .transpose()?
            .flatten();
        let binding = json!({"review":binding,"classifier":policy.get("classifier")});
        let epoch = self.registered_review_epoch(&canonical, context, &binding)?;
        let _claim = self.claim_fact_read(cancel, limits.deadline_unix_ms)?;
        let mut budget = crate::scan::ScanBudget::with_context(self, limits, context.clone());
        let mut stream = SourceStream::open(&canonical, 0, &mut budget, cancel)?;
        if stream.stamp.size > self.config.source as u64 {
            return Err(
                SnapshotError::new(Status::SourceLimit, "source exceeds owner bound").into(),
            );
        }
        if let Some(expected) = expected_revision {
            let state = self.lock_state();
            let matches = state
                .review_inputs
                .get(&epoch)
                .is_some_and(|input| input.scan_complete && input.source_revision() == expected);
            if !matches {
                return Err(SnapshotError::new(
                    Status::StaleHandle,
                    "prepared review source revision is unavailable",
                )
                .into());
            }
        }
        let (mut prior, mut reservation) = self.take_review_input(&epoch);
        let mut unchanged = false;
        let mut start = 0;
        if let Some(input) = &prior {
            match input.checkpoint.change(stream.stamp, &epoch) {
                RegisteredReviewChange::Unchanged => {
                    unchanged = input.scan_complete;
                    start = input.checkpoint.committed;
                }
                RegisteredReviewChange::Append { offset } => {
                    let boundary = LineSpan {
                        offset: input.checkpoint.stamp.size
                            - input.checkpoint.boundary.len() as u64,
                        len: input.checkpoint.boundary.len(),
                        terminated: false,
                    };
                    let observed = stream.read_span(&boundary, &mut budget, cancel)?;
                    if input.checkpoint.boundary_matches(&observed) {
                        unchanged = expected_revision.is_some() && input.scan_complete;
                        start = offset;
                    } else {
                        prior = None;
                        budget.progress.cache_invalidations += 1;
                    }
                }
                RegisteredReviewChange::Invalidated => {
                    prior = None;
                    budget.progress.cache_invalidations += 1;
                }
            }
        }
        if expected_revision.is_some() && prior.is_none() {
            return Err(SnapshotError::new(
                Status::Changed,
                "prepared review source revision changed",
            )
            .into());
        }
        let mut input = match prior {
            Some(input) => {
                budget.progress.cache_hits += 1;
                input
            }
            None => {
                self.review_reserve(
                    &mut reservation,
                    context,
                    size_of::<RegisteredReviewInput>()
                        + canonical.as_os_str().len()
                        + ActivityIndex::EMPTY_ALLOCATION_BYTES
                        + first_user_token.len() * (size_of::<usize>() + 2),
                )?;
                let mining = crate::mining::ReviewMiningIndex::new(spec)?;
                let facts = crate::snapshot_text::SourceFactsIndex::new(first_user_token);
                let input = RegisteredReviewInput {
                    checkpoint: RegisteredReviewCheckpoint {
                        stamp: stream.stamp,
                        committed: 0,
                        events: 0,
                        boundary: Vec::new(),
                        epoch,
                    },
                    mining,
                    facts,
                    activity: ActivityIndex::default(),
                    entries: Vec::new(),
                    handler_limits: limits,
                    preparation_progress: crate::scan::ScanProgress::default(),
                    preparation_usage: json!(null),
                    preparation_decoded: 0,
                    preparation_lifts: 0,
                    codex: None,
                    provider: None,
                    raw_spans: Vec::new(),
                    pending_rows: VecDeque::new(),
                    pending_row_bytes: 0,
                    pending_codex: 0..0,
                    canonical_path: canonical.clone(),
                    session_id: String::new(),
                    meta_events: Vec::new(),
                    model_events: Vec::new(),
                    retained_entries: 0,
                    pending_line: Vec::new(),
                    scan_complete: false,
                    first_user_token: first_user_token.to_owned(),
                    hook_markers: Vec::new(),
                    digests: HashMap::new(),
                    digest_bytes: 0,
                    contexts: crate::snapshot_review_context::ReviewContextState::default(),
                    spans: Vec::new(),
                    touched: 0,
                };
                self.review_reserve(&mut reservation, context, input.accounted_bytes())?;
                input
            }
        };
        if !unchanged
            && (input.checkpoint.boundary.is_empty() || input.checkpoint.stamp != stream.stamp)
        {
            let len = (stream.stamp.size as usize).min(64);
            let boundary = LineSpan {
                offset: stream.stamp.size - len as u64,
                len,
                terminated: false,
            };
            self.review_reserve(&mut reservation, context, input.accounted_bytes() + len)?;
            input.checkpoint.boundary = stream.read_span(&boundary, &mut budget, cancel)?;
        }
        input.preparation_decoded = 0;
        input.preparation_lifts = 0;
        if !unchanged {
            input.scan_complete = false;
            stream.restore_pending(start, &input.pending_line, &mut budget, cancel)?;
            input.pending_line.clear();
            let original_limits = budget.limits;
            budget.limits.max_source_read_bytes =
                budget.limits.max_source_read_bytes.saturating_sub(128);
            budget.limits.max_read_bytes = budget.limits.max_read_bytes.saturating_sub(128);
            loop {
                if let Err(error) = budget.checkpoint(cancel) {
                    if error.status == Status::Deadline {
                        break;
                    }
                    return Err(error.into());
                }
                let remaining = budget.remaining();
                if remaining.max_events <= input.mining.pending_count() * 3 + 8
                    || remaining.max_read_bytes <= 512
                {
                    break;
                }
                if !input.pending_codex.is_empty() {
                    let index = input.pending_codex.start;
                    let codex = input.codex.as_ref().expect("pending Codex source");
                    let line = input.raw_spans[codex.source_line(index)];
                    let bound = codex.entry_bound(index)
                        + dom_parse_bound(line.len)
                            .ok_or_else(|| invalid("Codex row bound overflow"))?;
                    self.review_reserve(
                        &mut reservation,
                        context,
                        input.accounted_bytes() + bound,
                    )?;
                    let entry = codex.canonical_entry(index);
                    let folded = self.fold_registered_review_entry(
                        &mut input,
                        &entry,
                        line,
                        context,
                        spec,
                        &registry,
                        &mut budget,
                        cancel,
                        &mut reservation,
                        prose_marker,
                        classifier.as_ref(),
                    );
                    if let Err(error) = folded {
                        if matches!(&error, crate::mining::MiningError::Snapshot(error) if error.status == Status::Deadline)
                        {
                            break;
                        }
                        return Err(error);
                    }
                    input.pending_codex.start += 1;
                    self.review_settle(&mut reservation, input.accounted_bytes());
                    continue;
                }
                if let Some((entry, line)) = input.pending_rows.pop_front() {
                    let charge = entry_charge(&entry);
                    input.pending_row_bytes -=
                        charge.owned_capacity_bytes + charge.opaque_dom_accounted_bytes;
                    let folded = self.fold_registered_review_entry(
                        &mut input,
                        &entry,
                        line,
                        context,
                        spec,
                        &registry,
                        &mut budget,
                        cancel,
                        &mut reservation,
                        prose_marker,
                        classifier.as_ref(),
                    );
                    if let Err(error) = folded {
                        if matches!(&error, crate::mining::MiningError::Snapshot(error) if error.status == Status::Deadline)
                        {
                            input.pending_row_bytes +=
                                charge.owned_capacity_bytes + charge.opaque_dom_accounted_bytes;
                            input.pending_rows.push_front((entry, line));
                            break;
                        }
                        return Err(error);
                    }
                    self.review_settle(&mut reservation, input.accounted_bytes());
                    continue;
                }
                if remaining.max_source_read_bytes == 0 {
                    break;
                }
                let line = match stream.next_line(&mut budget, cancel) {
                    Ok(Some(line)) => line,
                    Ok(None) => {
                        input.scan_complete = true;
                        break;
                    }
                    Err(error) if error.status == Status::Deadline => break,
                    Err(error)
                        if error.status == Status::Incomplete
                            && (error.reason == "source_read_limit"
                                || error.reason.contains("budget")) =>
                    {
                        break;
                    }
                    Err(error) => return Err(error.into()),
                };
                if !line.terminated {
                    stream.defer(&line);
                    break;
                }
                if line.len.saturating_mul(3) > budget.remaining().max_read_bytes
                    && input.checkpoint.committed > start
                {
                    stream.defer(&line);
                    break;
                }
                if let Err(error) = budget.checkpoint(cancel) {
                    if error.status == Status::Deadline {
                        stream.defer(&line);
                        break;
                    }
                    return Err(error.into());
                }
                let bytes = stream.bytes(&line);
                input.preparation_decoded += bytes.len();
                self.review_reserve(
                    &mut reservation,
                    context,
                    input.accounted_bytes() + vec_reallocation_bound(&input.raw_spans, 1),
                )?;
                input.raw_spans.push(line);
                if line.len > self.config.entry {
                    return Err(SnapshotError::new(
                        Status::EntryLimit,
                        "source entry exceeds owner bound",
                    )
                    .into());
                }
                if input.provider.is_none() && !bytes.iter().all(u8::is_ascii_whitespace) {
                    input.provider = Some(sniff_provider(bytes));
                }
                let mut entries = if input.provider == Some(Provider::Codex) {
                    let peak = match input.codex.as_ref() {
                        Some(codex) => codex.ingest_peak_bound(bytes.len() + 1),
                        None => crate::codex::CodexReviewContinuation::default()
                            .ingest_peak_bound(bytes.len() + 1),
                    }
                    .ok_or_else(|| invalid("Codex allocation bound overflow"))?;
                    self.review_reserve(&mut reservation, context, input.accounted_bytes() + peak)?;
                    let mut bytes = bytes.to_vec();
                    bytes.push(b'\n');
                    let codex = input.codex.get_or_insert_with(Default::default);
                    let changed = codex.ingest(&bytes)?;
                    let end = codex.len();
                    let start = if changed < input.entries.len() {
                        self.truncate_registered_review_input(
                            &mut input,
                            changed,
                            spec,
                            context,
                            &mut reservation,
                        )?;
                        changed
                    } else {
                        input.entries.len().min(changed)
                    };
                    input.pending_codex = start..end;
                    budget.commit_parse()?;
                    Vec::new()
                } else {
                    let rows = self.review_decode(
                        bytes,
                        context,
                        &mut reservation,
                        input.accounted_bytes(),
                    )?;
                    for _ in &rows {
                        budget.commit_parse()?;
                    }
                    rows.into_iter()
                        .map(|entry| (entry, line))
                        .collect::<Vec<_>>()
                };
                self.review_reserve(
                    &mut reservation,
                    context,
                    input.accounted_bytes()
                        + entries
                            .iter()
                            .map(|(entry, _)| {
                                let charge = entry_charge(entry);
                                charge.owned_capacity_bytes
                                    + charge.opaque_dom_accounted_bytes
                                    + size_of::<(Entry, crate::scan_stream::LineSpan)>()
                            })
                            .sum::<usize>()
                        + deque_reallocation_bound(&input.pending_rows, entries.len()),
                )?;
                for (entry, line) in entries.drain(..) {
                    let charge = entry_charge(&entry);
                    input.pending_row_bytes +=
                        charge.owned_capacity_bytes + charge.opaque_dom_accounted_bytes;
                    input.pending_rows.push_back((entry, line));
                }
                input.checkpoint.committed = line.end();
            }
            budget.limits = original_limits;
            let pending_bytes = stream.pending();
            self.review_reserve(
                &mut reservation,
                context,
                input.accounted_bytes() + pending_bytes.len(),
            )?;
            input.pending_line = pending_bytes.to_vec();
            input.checkpoint.events = input.mining.event_count();
            input.checkpoint.stamp = stream.stamp;
        }
        let mut mining_usage = crate::mining::MiningUsage::default();
        if cancel.check(limits.deadline_unix_ms).is_ok() {
            let finalized = (|| -> crate::mining::MiningResult<()> {
                let pending = input.mining.pending_count();
                let targets = pending + input.contexts.pending_count();
                self.review_reserve(
                    &mut reservation,
                    context,
                    input.accounted_bytes()
                        + targets.max(4) * 4 * size_of::<usize>()
                        + targets * size_of::<(usize, Entry)>(),
                )?;
                let mut required = input.mining.required_payloads(spec)?;
                required.reserve(input.contexts.pending_count());
                required.extend(input.contexts.required_payloads(&input.activity));
                required.sort_unstable();
                required.dedup();
                let mut payloads = Vec::with_capacity(required.len());
                let mut payload_bytes = required.capacity() * size_of::<usize>()
                    + payloads.capacity() * size_of::<(usize, Entry)>();
                for event in required {
                    let span = input
                        .event_span(event)
                        .expect("registered review event span");
                    if let Some(codex) = input.codex.as_ref() {
                        let entry = codex.canonical_entry(event);
                        let charge = entry_charge(&entry);
                        payload_bytes +=
                            charge.owned_capacity_bytes + charge.opaque_dom_accounted_bytes;
                        payloads.push((event, entry));
                        continue;
                    }
                    let bytes = stream.read_span(&span, &mut budget, cancel)?;
                    let entries = self.review_decode(
                        &bytes,
                        context,
                        &mut reservation,
                        input.accounted_bytes() + payload_bytes + bytes.capacity(),
                    )?;
                    input.preparation_decoded += bytes.len();
                    budget.commit_parse()?;
                    let mut entries = entries.into_iter();
                    let entry = entries.next().expect("recorded Claude review event");
                    assert!(
                        entries.next().is_none(),
                        "one Claude event per recorded span"
                    );
                    let charge = entry_charge(&entry);
                    payload_bytes += charge.owned_capacity_bytes
                        + charge.opaque_dom_accounted_bytes
                        + dom_parse_bound(span.len)
                            .ok_or_else(|| invalid("review payload allocation bound overflow"))?;
                    payloads.push((event, entry));
                }
                self.review_reserve(
                    &mut reservation,
                    context,
                    input.accounted_bytes()
                        + payload_bytes.saturating_mul(2)
                        + usize::from(pending != 0) * limits.max_output_bytes.saturating_mul(3)
                        + pending * size_of::<(usize, &Entry)>(),
                )?;
                let overrides = payloads
                    .iter()
                    .map(|(event, entry)| (*event, entry))
                    .collect();

                match input.mining.process(
                    spec,
                    &overrides,
                    budget.remaining(),
                    cancel,
                    &mut mining_usage,
                ) {
                    Ok(()) => {}
                    Err(crate::mining::MiningError::Snapshot(error))
                        if error.status == Status::Incomplete
                            || error.status == Status::OutputLimit
                            || error.status == Status::Deadline =>
                    {
                        input.scan_complete = false;
                    }
                    Err(error) => return Err(error),
                }
                input.scan_complete &= input.pending_rows.is_empty()
                    && input.pending_codex.is_empty()
                    && input.mining.pending_count() == 0;
                input.contexts.resolve(&input.activity, &overrides);
                budget.commit_projection(mining_usage.input_bytes, mining_usage.events)?;
                drop(overrides);
                drop(payloads);
                self.review_settle(&mut reservation, input.accounted_bytes());
                Ok(())
            })();
            match finalized {
                Ok(()) => {}
                Err(crate::mining::MiningError::Snapshot(error))
                    if error.status == Status::Deadline
                        || error.status == Status::Incomplete
                        || error.status == Status::OutputLimit =>
                {
                    input.scan_complete = false;
                    self.review_settle(&mut reservation, input.accounted_bytes());
                }
                Err(error) => return Err(error),
            }
        } else {
            cancel.check(u64::MAX)?;
            input.scan_complete = false;
        }
        input.handler_limits = budget.remaining();
        input.preparation_progress = budget.progress.clone();
        let mut usage = [0u64; 20];
        usage[0] = budget.progress.source_opens as u64;
        usage[1] = budget.progress.source_bytes as u64;
        usage[2] = input.preparation_decoded as u64;
        usage[3] = budget.progress.parsed_events as u64;
        usage[4] = u64::from(input.preparation_decoded != 0 && budget.progress.cache_hits == 0);
        usage[5] = u64::from(input.preparation_decoded != 0 && budget.progress.cache_hits != 0);
        usage[6] = input.preparation_lifts as u64;
        usage[7] = budget.progress.cache_hits as u64;
        usage[10] = budget.progress.cache_invalidations as u64;
        self.review_reserve(&mut reservation, context, input.accounted_bytes() + 8192)?;
        input.preparation_usage = usage_value(&usage);
        self.review_settle(&mut reservation, input.accounted_bytes());
        Ok(RegisteredReviewPending {
            store: self,
            canonical,
            context,
            input: Some(input),
            stream,
            budget,
            reservation,
            mining_usage,
            _claim,
        })
    }
}

fn registered_review_entry(mut entry: Entry, marked: bool) -> Result<Entry, SnapshotError> {
    fn compact(blocks: &mut Vec<crate::types::ContentBlock>) {
        for block in blocks {
            if let crate::types::ContentBlock::ToolResult(result) = block {
                if !result.is_error {
                    result.content.clear();
                    result.content.shrink_to_fit();
                    result.tool_use_result = None;
                }
            }
        }
    }
    match &mut entry {
        Entry::User(user) => {
            if let crate::types::UserContent::Blocks(blocks) = &mut user.content {
                compact(blocks);
            }
        }
        Entry::Assistant(assistant) => {
            compact(&mut assistant.blocks);
            if !marked {
                for block in &mut assistant.blocks {
                    if let crate::types::ContentBlock::Text(text)
                    | crate::types::ContentBlock::Thinking(text) = block
                    {
                        text.clear();
                        text.shrink_to_fit();
                    }
                }
            }
        }
        _ => {}
    }
    let bytes = sonic_rs::to_vec(&entry).map_err(|error| invalid(error.to_string()))?;
    sonic_rs::from_slice(&bytes).map_err(|error| invalid(error.to_string()))
}
