struct CompactCodexAppendBound {
    successor: usize,
    append_peak: usize,
    lower_peak: usize,
    inherited_strings: usize,
}

impl CodexAppendIndex {
    fn compact_append_bound(&self, suffix: &CodexSession) -> CompactCodexAppendBound {
        let (mut users, mut assistants, mut user_bytes, mut assistant_bytes) = (0, 0, 0, 0);
        let (mut user_events, mut assistant_events, mut block_refs) = (0, 0, 0);
        let mut cwd = self.cwd.as_ref().map_or(0, String::capacity);
        let mut model = self.model.as_ref().map_or(0, String::capacity);
        let (mut metadata_bytes, mut model_bytes, mut models) = (0, 0, 0);
        for entry in &suffix.entries {
            match &entry.item {
                CodexItem::ResponseItem(item) => {
                    if let ResponseItemPayload::Message { role, content, .. } = &item.payload {
                        let count = content.iter().filter_map(|block| crate::value::field_str(block, "text")).count();
                        let bytes = content.iter().filter_map(|block| crate::value::field_str(block, "text"))
                            .map(str::len).sum::<usize>() + count.saturating_sub(1);
                        block_refs += crate::parse::pushed_capacity(count) * size_of::<&str>();
                        match role.as_deref() {
                            Some("user") => { users += 1; user_bytes += bytes; }
                            Some("assistant") => { assistants += 1; assistant_bytes += bytes; }
                            _ => {}
                        }
                    }
                }
                CodexItem::EventMsg(EventMsg::UserMessage { .. }) => user_events += 1,
                CodexItem::EventMsg(EventMsg::AgentMessage { .. }) => assistant_events += 1,
                CodexItem::TurnContext(value) => {
                    if let Some(next) = crate::value::field_str(value, "cwd") {
                        cwd = cwd.max(next.len());
                        metadata_bytes += next.len();
                    }
                    if let Some(next) = crate::value::field_str(value, "model") {
                        model = model.max(next.len());
                        metadata_bytes += next.len();
                        model_bytes += next.len();
                        models += 1;
                    }
                }
                _ => {}
            }
        }
        let successor = self.accounted_bytes()
            + set_growth(&self.user_echoes, users)
            + set_growth(&self.assistant_echoes, assistants)
            + set_growth(&self.user_events, user_events)
            + set_growth(&self.assistant_events, assistant_events)
            + user_bytes + assistant_bytes + metadata_bytes;
        let old_tables = (self.user_echoes.capacity() + self.assistant_echoes.capacity()) * size_of::<String>()
            + (self.user_events.capacity() + self.assistant_events.capacity()) * size_of::<[u8; 32]>();
        let old_echo_tables = (self.user_echoes.capacity() + self.assistant_echoes.capacity()) * size_of::<String>();
        let grown_echo_tables = (set_capacity_for(&self.user_echoes, users)
            + set_capacity_for(&self.assistant_echoes, assistants)) * size_of::<String>();
        let new_echo_tables = [users, assistants].into_iter().filter(|count| *count != 0)
            .map(|count| hashbrown_tier(count, size_of::<String>()) * size_of::<String>()).sum::<usize>();
        let old_echo_strings = self.user_echoes.iter().chain(&self.assistant_echoes)
            .map(String::capacity).sum::<usize>();
        let model_slots = crate::parse::pushed_capacity(models + usize::from(self.model.is_some()));
        CompactCodexAppendBound {
            successor,
            append_peak: successor + old_tables + block_refs,
            lower_peak: old_echo_tables + grown_echo_tables + 2 * new_echo_tables
                + old_echo_strings + user_bytes + assistant_bytes + block_refs
                + model_slots * size_of::<(usize, String)>()
                + self.model.as_ref().map_or(0, String::capacity) + model_bytes + cwd,
            inherited_strings: self.thread_id.as_ref().map_or(0, String::capacity) + cwd + model,
        }
    }
}

#[derive(Clone)]
struct CodexIndexReducer {
    calls: HashSet<String>,
    results: HashMap<String, bool>,
}

impl CodexIndexReducer {
    fn empty() -> Self {
        Self { calls: HashSet::new(), results: HashMap::new() }
    }

    fn extend<'a>(&mut self, entries: impl Iterator<Item = &'a Entry>) {
        for entry in entries {
            match entry {
                Entry::Assistant(assistant) => {
                    for block in &assistant.blocks {
                        if let crate::types::ContentBlock::ToolUse(call) = block {
                            self.calls.insert(call.id.clone());
                        }
                    }
                }
                Entry::User(user) => {
                    for result in user.tool_results() {
                        self.results.insert(result.tool_use_id.clone(), result.is_error);
                    }
                }
                _ => {}
            }
        }
    }

    fn has_error(&self) -> bool {
        self.calls.iter().any(|id| self.results.get(id) == Some(&true))
    }

    fn accounted_bytes(&self) -> usize {
        self.calls.capacity() * size_of::<String>()
            + self.calls.iter().map(String::capacity).sum::<usize>()
            + self.results.capacity() * size_of::<(String, bool)>()
            + self.results.keys().map(String::capacity).sum::<usize>()
    }

    fn extend_bound<'a>(&self, entries: impl Iterator<Item = &'a Entry>) -> usize {
        let (mut calls, mut results, mut strings) = (0, 0, 0);
        for entry in entries {
            match entry {
                Entry::Assistant(assistant) => {
                    for block in &assistant.blocks {
                        if let crate::types::ContentBlock::ToolUse(call) = block {
                            calls += 1;
                            strings += call.id.len();
                        }
                    }
                }
                Entry::User(user) => {
                    for result in user.tool_results() {
                        results += 1;
                        strings += result.tool_use_id.len();
                    }
                }
                _ => {}
            }
        }
        self.accounted_bytes() + strings
            + map_growth(&self.results, results)
            + set_growth(&self.calls, calls)
    }
}

struct CompactCodexIndex {
    stamp: SourceStamp,
    fence: Vec<u8>,
    session_id: String,
    index: CodexAppendIndex,
    reducer: CodexIndexReducer,
}

impl CompactCodexIndex {
    fn accounted_bytes(&self) -> usize {
        arc_bytes::<Self>() + self.fence.capacity() + self.session_id.capacity()
            + self.index.accounted_bytes() - arc_bytes::<CodexAppendIndex>()
            + self.reducer.accounted_bytes()
    }
}

fn append_anchor(append: &Arc<CompactCodexIndex>) -> Anchor {
    Anchor::facts(Arc::as_ptr(append) as usize, append.accounted_bytes())
}

struct RetainedCompactCodex<'a> {
    store: &'a NativeStore,
    append: Arc<CompactCodexIndex>,
    facts: RetainedFacts<'a>,
}

impl Drop for RetainedCompactCodex<'_> {
    fn drop(&mut self) {
        let mut state = self.store.lock_state();
        state.ledger.shared.release(Arc::as_ptr(&self.append) as usize);
        #[cfg(test)]
        {
            let at = state.retained_append_owners.iter()
                .position(|owner| owner.as_ptr() == Arc::as_ptr(&self.append))
                .expect("retained append owner");
            state.retained_append_owners.swap_remove(at);
        }
    }
}

impl NativeStore {
    fn capture_compact_codex(
        &self,
        snapshot: &TranscriptSnapshot,
        context: &Value,
        cancel: &Cancellation,
        deadline: u64,
        reservation: &mut ProjectionReservation<'_>,
    ) -> Result<Option<Arc<CompactCodexIndex>>, SnapshotError> {
        let Some(index) = snapshot.codex_append.as_ref().filter(|index| {
            snapshot.provider == Provider::Codex && snapshot.window_start == 0
                && !snapshot.provisional_tail && snapshot.committed_bytes == snapshot.stamp.size
                && index.terminated
        }) else { return Ok(None) };
        cancel.check(deadline)?;
        let Some((_, fence)) = snapshot.prefix_fence() else { return Ok(None) };
        let empty = CodexIndexReducer::empty();
        let bound = arc_bytes::<CompactCodexIndex>() + index.accounted_bytes()
            + fence.len() + snapshot.session_id.len()
            + empty.extend_bound(snapshot.chunks.iter().flat_map(|chunk| chunk.entries.iter()));
        if bound > self.prepared_fact_budget() {
            return Ok(None);
        }
        match self.extend_projection_reservation(reservation, context, bound) {
            Ok(()) => {}
            Err(error) if error.status == Status::RetainedLimit => return Ok(None),
            Err(error) => return Err(error),
        }
        let mut reducer = empty;
        reducer.extend(snapshot.chunks.iter().flat_map(|chunk| chunk.entries.iter()));
        let append = Arc::new(CompactCodexIndex {
            stamp: snapshot.stamp, fence: fence.to_vec(), session_id: snapshot.session_id.clone(),
            index: index.as_ref().clone(), reducer,
        });
        assert!(append.accounted_bytes() <= bound, "compact Codex capture exceeded admitted bytes");
        cancel.check(deadline)?;
        Ok(Some(append))
    }

    fn previous_compact_codex(
        &self,
        stamp: SourceStamp,
        context: &Value,
        classifier: &Value,
    ) -> Result<Option<RetainedCompactCodex<'_>>, SnapshotError> {
        let registry = str_field(context, "registry_generation")?;
        let admission = str_field(context, "admission")?;
        let mut state = self.lock_state();
        if state.latest_source(&stamp.identity).is_some_and(|snapshot| snapshot.codex_raw.is_some()) {
            return Ok(None);
        }
        let candidate = state.identity_facts(stamp.identity)
            .filter(|(_, cached)| {
                cached.registry_generation == registry && cached.admission == admission
                    && cached.authority == context["authority"] && cached.classifier == *classifier
                    && cached.append.as_ref().is_some_and(|append| {
                        append.stamp == cached.stamp && append.stamp.size < stamp.size
                            && append.stamp.identity.window_base == 0
                    })
            })
            .max_by_key(|(_, cached)| cached.stamp.size)
            .map(|(_, cached)| (Arc::clone(&cached.facts), Arc::clone(cached.append.as_ref().unwrap())));
        let Some((facts, append)) = candidate else { return Ok(None) };
        state.ledger.shared.acquire(facts_anchor(&facts));
        state.ledger.shared.acquire(append_anchor(&append));
        #[cfg(test)]
        state.retained_append_owners.push(Arc::downgrade(&append));
        let facts = state.retained_facts(self, facts);
        Ok(Some(RetainedCompactCodex { store: self, append, facts }))
    }

    fn append_compact_codex(
        &self,
        canonical: &Path,
        stamp: SourceStamp,
        context: &Value,
        classifier: &Value,
        remaining: &mut WorkLimits,
        cancel: &Cancellation,
        usage: &mut [u64; 20],
    ) -> Result<Option<PreparedSourceOutcome<'_>>, SnapshotError> {
        let Some(previous) = self.previous_compact_codex(stamp, context, classifier)? else {
            return Ok(None);
        };
        let prior = &previous.append;
        let suffix_size: usize = (stamp.size - prior.stamp.size).try_into().map_err(|_| source_read_limit())?;
        let read_bytes = suffix_size.saturating_add(prior.fence.len()).saturating_add((stamp.size as usize).min(64).saturating_mul(2));
        if suffix_size > self.config.read_step || read_bytes > remaining.max_source_read_bytes {
            return Ok(None);
        }
        if stamp.size > self.config.source as u64 {
            return Err(SnapshotError::new(Status::SourceLimit, "source exceeds owner bound"));
        }
        cancel.check(remaining.deadline_unix_ms)?;
        self.authority(context, Some(canonical))?;
        let mut file = File::open(canonical).map_err(io_error)?;
        usage[0] += 1;
        if SourceStamp::of(&file.metadata().map_err(io_error)?) != stamp
            || SourceStamp::of(&std::fs::metadata(canonical).map_err(io_error)?) != stamp {
            return Err(SnapshotError::new(Status::Changed, "prepared append changed during open"));
        }
        let mut reservation = self.reserve_projection(context, read_bytes)?;
        let mut origin = vec![0; (stamp.size as usize).min(64)];
        file.seek(SeekFrom::Start(stamp.size - origin.len() as u64)).map_err(io_error)?;
        self.before_source_read();
        file.read_exact(&mut origin).map_err(io_error)?;
        usage[1] += origin.len() as u64;
        #[cfg(test)]
        self.observe_hydration(HydrationObservation::Read { stamp, bytes: origin.len() as u64 });
        remaining.max_source_read_bytes -= origin.len();
        let mut fence = vec![0; prior.fence.len()];
        file.seek(SeekFrom::Start(prior.stamp.size - fence.len() as u64)).map_err(io_error)?;
        self.before_source_read();
        file.read_exact(&mut fence).map_err(io_error)?;
        usage[1] += fence.len() as u64;
        #[cfg(test)]
        self.observe_hydration(HydrationObservation::Read { stamp, bytes: fence.len() as u64 });
        remaining.max_source_read_bytes -= fence.len();
        if fence != prior.fence {
            return Ok(None);
        }
        let mut suffix = vec![0; suffix_size];
        file.seek(SeekFrom::Start(prior.stamp.size)).map_err(io_error)?;
        self.before_source_read();
        file.read_exact(&mut suffix).map_err(io_error)?;
        usage[1] += suffix_size as u64;
        #[cfg(test)]
        self.observe_hydration(HydrationObservation::Read { stamp, bytes: suffix_size as u64 });
        remaining.max_source_read_bytes -= suffix_size;
        let mut start = 0;
        for end in memchr::memchr_iter(b'\n', &suffix).chain(std::iter::once(suffix.len())) {
            if end - start > self.config.entry {
                return Err(SnapshotError::new(Status::EntryLimit, "source entry exceeds owner bound"));
            }
            start = end + 1;
        }
        if !suffix.ends_with(b"\n") { return Ok(None) }
        let source_lines = memchr::memchr_iter(b'\n', &suffix).count();
        let mut decode_bound = suffix.len().saturating_mul(4);
        for line in suffix.split(|byte| *byte == b'\n').filter(|line| !line.iter().all(u8::is_ascii_whitespace)) {
            decode_bound = decode_bound.saturating_add(
                dom_parse_bound(line.len()).ok_or_else(|| invalid("Codex append bound overflow"))?.saturating_mul(2));
        }
        let slots = crate::parse::pushed_capacity(source_lines).max(suffix.len() / 700 + 1);
        decode_bound = decode_bound.saturating_add(slots.saturating_mul(size_of::<CodexEntry>()));
        self.extend_projection_reservation(&mut reservation, context, decode_bound)?;
        let mut parsed = parse_codex_bytes(&suffix);
        usage[2] += suffix_size as u64;
        for entry in &mut parsed.entries { entry.line_index += prior.index.lines; }
        if prior.index.fallback_reason(&parsed).is_some() { return Ok(None) }
        #[cfg(test)]
        self.observe_hydration(HydrationObservation::Codex {
            stamp, previous: false, raw: false, index: true, append: true, fallback: None, suffix_bytes: suffix_size,
        });
        if parsed.entries.len() > remaining.max_events {
            return Err(SnapshotError::new(Status::EntryLimit, "lowered provider entries exceed work bound"));
        }
        cancel.check(remaining.deadline_unix_ms)?;
        let registry = self.registry(context)?;
        let append_bound = prior.index.compact_append_bound(&parsed);
        let lowering_bound = suffix_size.saturating_mul(128)
            .saturating_add(parsed.entries.len().saturating_mul(size_of::<Entry>()))
            .saturating_add((parsed.entries.len() + 1).saturating_mul(append_bound.inherited_strings))
            .saturating_add(append_bound.lower_peak);
        self.extend_projection_reservation(&mut reservation, context, lowering_bound)?;
        let entries = prior.index.lower_suffix(&parsed);
        if entries.len().saturating_mul(2) > remaining.max_events {
            return Err(SnapshotError::new(Status::SourceLimit, "cumulative preparation work budget exhausted"));
        }
        usage[3] += entries.len() as u64;
        remaining.max_events -= entries.len() * 2;
        let reducer_bound = prior.reducer.extend_bound(entries.iter());
        let index_bound = append_bound.successor;
        self.extend_projection_reservation(&mut reservation, context,
            reducer_bound.saturating_add(append_bound.append_peak).saturating_add(arc_bytes::<CompactCodexIndex>())
                .saturating_add(origin.len()).saturating_add(prior.session_id.len()))?;
        let mut reducer = prior.reducer.clone();
        reducer.extend(entries.iter());
        let index = prior.index.append(&parsed, &suffix);
        assert!(index.accounted_bytes() <= index_bound, "compact index exceeded admitted bytes");
        assert!(reducer.accounted_bytes() <= reducer_bound, "compact reducer exceeded admitted bytes");
        let snapshot = crate::toolcall::with_registry(Arc::clone(&registry), || {
            TranscriptSnapshot::from_complete_entries(String::new(), PathBuf::new(), stamp,
                Provider::Codex, prior.session_id.clone(), entries)
        });
        let bound = crate::snapshot_projection::facts_bound(&snapshot, FactsNeed::Index);
        let prefix = previous.facts.facts();
        self.extend_projection_reservation(&mut reservation, context,
            bound.saturating_add(prefix.accounted_bytes()).saturating_mul(2))?;
        let mut fact_limits = *remaining;
        fact_limits.max_read_bytes = self.config.source;
        fact_limits.max_events = snapshot.event_count;
        #[cfg(test)]
        self.fact_builds.fetch_add(1, Ordering::Relaxed);
        let (next, _, _) = crate::toolcall::with_registry(registry, || {
            crate::snapshot_projection::prepare_facts(&snapshot, &json!([]), &fact_limits, cancel, FactsNeed::Index)
        })?;
        let mut inputs = json!({});
        for column in ["calls", "commands", "edited_files", "skills"] {
            let values: Vec<&Value> = prefix.inputs[column].as_array().expect("prepared Index column")
                .iter().chain(next.inputs[column].as_array().expect("prepared Index column").iter()).collect();
            inputs.insert(column, sonic_rs::to_value(&values).map_err(|error| invalid(error.to_string()))?);
        }
        crate::snapshot_codec::write_json(&mut std::io::sink(), &inputs, 16 * 1024 * 1024)
            .map_err(|_| SnapshotError::new(Status::Incomplete, "prepared predicate facts exceed source bound"))?;
        let mut facts = crate::snapshot_prepared::PreparedFacts {
            inputs, has_error: reducer.has_error(), overrides: crate::snapshot_prepared::OverrideFacts::Unloaded, accounted: 0,
        };
        facts.refresh_accounted();
        let append = Arc::new(CompactCodexIndex { stamp, fence: origin, session_id: prior.session_id.clone(), index, reducer });
        let facts = Arc::new(facts);
        cancel.check(remaining.deadline_unix_ms)?;
        self.authority(context, Some(canonical))?;
        if SourceStamp::of(&file.metadata().map_err(io_error)?) != stamp
            || SourceStamp::of(&std::fs::metadata(canonical).map_err(io_error)?) != stamp {
            return Err(SnapshotError::new(Status::Changed, "prepared append changed before publication"));
        }
        let mut seal = vec![0; (stamp.size as usize).min(64)];
        file.seek(SeekFrom::Start(stamp.size - seal.len() as u64)).map_err(io_error)?;
        self.before_source_read();
        file.read_exact(&mut seal).map_err(io_error)?;
        usage[1] += seal.len() as u64;
        remaining.max_source_read_bytes -= seal.len();
        #[cfg(test)]
        self.observe_hydration(HydrationObservation::Read { stamp, bytes: seal.len() as u64 });
        cancel.check(remaining.deadline_unix_ms)?;
        let current_path = self.canonical_path(canonical).map_err(io_error)?;
        self.authority(context, Some(&current_path))?;
        if seal != append.fence
            || current_path != canonical
            || SourceStamp::of(&file.metadata().map_err(io_error)?) != stamp
            || SourceStamp::of(&std::fs::metadata(&current_path).map_err(io_error)?) != stamp
        {
            return Err(SnapshotError::new(Status::Changed, "prepared append seal changed"));
        }
        cancel.check(remaining.deadline_unix_ms)?;
        let key = PreparedDiskKey::new(stamp, str_field(context, "registry_generation")?,
            str_field(context, "admission")?, &context["authority"], classifier)?;
        if self.prepared_disk.insert(&key, &facts,
            |bytes| self.extend_projection_reservation(&mut reservation, context, bytes).is_ok(),
            |growth| self.admit_prepared_disk_growth(context, growth))?
            == crate::snapshot_prepared_disk::Inserted::AlreadyFull {
            return Ok(Some(PreparedSourceOutcome::Superseded));
        }
        let held = match self.cache_prepared_facts(stamp, Arc::clone(&facts), classifier, context,
            &mut reservation, FactsNeed::Index, Some(append)) {
            Ok(retained) => HeldFacts::Retained(retained),
            Err(error) if error.status == Status::RetainedLimit => HeldFacts::Reserved { facts, reservation },
            Err(error) => return Err(error),
        };
        self.lock_state().prepared_loads.remove(&stamp.identity);
        Ok(Some(PreparedSourceOutcome::Ready { stamp, held, cached: false }))
    }
}

#[cfg(test)]
impl NativeStore {
    fn audit_compact_codex_bytes(append: &CompactCodexIndex) -> usize {
        let CompactCodexIndex { stamp: _, fence, session_id, index, reducer } = append;
        let CodexIndexReducer { calls, results } = reducer;
        arc_mirror::<CompactCodexIndex>() + fence.capacity() + session_id.capacity()
            + Self::audit_codex_index_bytes(index) - arc_mirror::<CodexAppendIndex>()
            + calls.capacity() * size_of::<String>()
            + calls.iter().map(String::capacity).sum::<usize>()
            + results.capacity() * size_of::<(String, bool)>()
            + results.keys().map(String::capacity).sum::<usize>()
    }
}
