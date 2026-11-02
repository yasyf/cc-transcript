use crate::codex::lower::{blocks_text, echo_sets, lower_entry, session_arenas};
use crate::codex::types::{CodexEntry, CodexItem, CodexSession, EventMsg, ResponseItemPayload};
use crate::codex::{lower, parse_codex_bytes};

#[derive(Debug, Clone)]
pub(crate) struct CodexAppendIndex {
    thread_id: Option<String>,
    cwd: Option<String>,
    model: Option<String>,
    lines: usize,
    terminated: bool,
    last_trigger_turn: bool,
    user_echoes: HashSet<String>,
    assistant_echoes: HashSet<String>,
    user_events: HashSet<[u8; 32]>,
    assistant_events: HashSet<[u8; 32]>,
}

impl CodexAppendIndex {
    fn from_session(session: &CodexSession, raw: &[u8]) -> Self {
        let (user_echoes, assistant_echoes) = echo_sets(session);
        let mut cwd = session.cwd.clone();
        let mut model = None;
        let mut user_events = HashSet::new();
        let mut assistant_events = HashSet::new();
        for entry in &session.entries {
            match &entry.item {
                CodexItem::TurnContext(value) => {
                    if let Some(next) = crate::value::field_str(value, "cwd") {
                        cwd = Some(next.to_owned());
                    }
                    if let Some(next) = crate::value::field_str(value, "model") {
                        model = Some(next.to_owned());
                    }
                }
                CodexItem::EventMsg(EventMsg::UserMessage { message, .. }) => {
                    user_events.insert(Self::digest(message.as_deref().unwrap_or("")));
                }
                CodexItem::EventMsg(EventMsg::AgentMessage { message, .. }) => {
                    assistant_events.insert(Self::digest(message.as_deref().unwrap_or("")));
                }
                _ => {}
            }
        }
        let lines = memchr::memchr_iter(b'\n', raw).count();
        let terminated = raw.ends_with(b"\n");
        let last_trigger_turn = terminated
            && session.entries.last().is_some_and(|entry| {
                entry.line_index + 1 == lines
                    && matches!(
                        &entry.item,
                        CodexItem::InterAgentCommunicationMetadata {
                            trigger_turn: Some(true)
                        }
                    )
            });
        Self {
            thread_id: session.rollout_thread_id.clone(),
            cwd,
            model,
            lines,
            terminated,
            last_trigger_turn,
            user_echoes,
            assistant_echoes,
            user_events,
            assistant_events,
        }
    }

    fn digest(value: &str) -> [u8; 32] {
        Sha256::digest(value.as_bytes()).into()
    }

    fn fallback_reason(&self, suffix: &CodexSession) -> Option<&'static str> {
        if !self.terminated {
            return Some("partial_tail");
        }
        for entry in &suffix.entries {
            match &entry.item {
                CodexItem::SessionMeta(meta)
                    if self.thread_id.is_none()
                        && meta.id.as_deref().is_some_and(|id| !id.is_empty()) =>
                {
                    return Some("first_session_meta");
                }
                CodexItem::TurnContext(value)
                    if self.model.is_none()
                        && crate::value::field_str(value, "model").is_some() =>
                {
                    return Some("first_model");
                }
                CodexItem::ResponseItem(item) => {
                    if let ResponseItemPayload::Message { role, content, .. } = &item.payload {
                        let text = blocks_text(content);
                        match role.as_deref() {
                            Some("user") if self.user_events.contains(&Self::digest(&text)) => {
                                return Some("user_echo");
                            }
                            Some("assistant")
                                if self.assistant_events.contains(&Self::digest(&text)) =>
                            {
                                return Some("assistant_echo");
                            }
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
        }
        None
    }

    fn append(&self, suffix: &CodexSession, raw: &[u8]) -> Self {
        let mut next = self.clone();
        next.lines += memchr::memchr_iter(b'\n', raw).count();
        next.terminated = raw.ends_with(b"\n");
        next.last_trigger_turn = next.terminated
            && suffix.entries.last().is_some_and(|entry| {
                entry.line_index + 1 == next.lines
                    && matches!(
                        &entry.item,
                        CodexItem::InterAgentCommunicationMetadata {
                            trigger_turn: Some(true)
                        }
                    )
            });
        for entry in &suffix.entries {
            match &entry.item {
                CodexItem::TurnContext(value) => {
                    if let Some(cwd) = crate::value::field_str(value, "cwd") {
                        next.cwd = Some(cwd.to_owned());
                    }
                    if let Some(model) = crate::value::field_str(value, "model") {
                        next.model = Some(model.to_owned());
                    }
                }
                CodexItem::EventMsg(EventMsg::UserMessage { message, .. }) => {
                    next.user_events
                        .insert(Self::digest(message.as_deref().unwrap_or("")));
                }
                CodexItem::EventMsg(EventMsg::AgentMessage { message, .. }) => {
                    next.assistant_events
                        .insert(Self::digest(message.as_deref().unwrap_or("")));
                }
                CodexItem::ResponseItem(item) => {
                    if let ResponseItemPayload::Message { role, content, .. } = &item.payload {
                        match role.as_deref() {
                            Some("user") => {
                                next.user_echoes.insert(blocks_text(content));
                            }
                            Some("assistant") => {
                                next.assistant_echoes.insert(blocks_text(content));
                            }
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
        }
        next
    }

    fn lower_suffix(&self, suffix: &CodexSession) -> Vec<Entry> {
        let mut user_echoes = self.user_echoes.clone();
        let mut assistant_echoes = self.assistant_echoes.clone();
        let (new_user_echoes, new_assistant_echoes) = echo_sets(suffix);
        user_echoes.extend(new_user_echoes);
        assistant_echoes.extend(new_assistant_echoes);
        let mut models = Vec::new();
        if let Some(model) = &self.model {
            models.push((0, model.clone()));
        }
        for (position, entry) in suffix.entries.iter().enumerate() {
            if let CodexItem::TurnContext(value) = &entry.item {
                if let Some(model) = crate::value::field_str(value, "model") {
                    models.push((position, model.to_owned()));
                }
            }
        }
        let previous = self.last_trigger_turn.then(|| CodexEntry {
            line_index: self.lines - 1,
            timestamp: None,
            source: None,
            item: CodexItem::InterAgentCommunicationMetadata {
                trigger_turn: Some(true),
            },
        });
        let mut cwd = self.cwd.clone();
        suffix
            .entries
            .iter()
            .enumerate()
            .map(|(position, entry)| {
                if let CodexItem::TurnContext(value) = &entry.item {
                    if let Some(next) = crate::value::field_str(value, "cwd") {
                        cwd = Some(next.to_owned());
                    }
                }
                let prior = if position == 0 {
                    previous.as_ref()
                } else {
                    suffix.entries.get(position - 1)
                };
                lower_entry(
                    entry,
                    position,
                    prior,
                    self.thread_id.as_deref().unwrap_or(""),
                    cwd.as_deref(),
                    &models,
                    &user_echoes,
                    &assistant_echoes,
                )
            })
            .collect()
    }

    pub(crate) fn accounted_bytes(&self) -> usize {
        arc_bytes::<Self>()
            + self.thread_id.as_ref().map_or(0, String::capacity)
            + self.cwd.as_ref().map_or(0, String::capacity)
            + self.model.as_ref().map_or(0, String::capacity)
            + self.user_echoes.capacity() * size_of::<String>()
            + self.user_echoes.iter().map(String::capacity).sum::<usize>()
            + self.assistant_echoes.capacity() * size_of::<String>()
            + self
                .assistant_echoes
                .iter()
                .map(String::capacity)
                .sum::<usize>()
            + (self.user_events.capacity() + self.assistant_events.capacity())
                * size_of::<[u8; 32]>()
    }
}

impl NativeStore {
    fn owned_agent_codex_entry(
        &self,
        slot: &LoadSlot,
        context: &Value,
        codex: &crate::codex::CodexReviewContinuation,
        position: usize,
    ) -> Result<Entry, SnapshotError> {
        let row_bound = codex.entry_bound(position);
        let canonical_bound = dom_parse_bound(row_bound)
            .and_then(|bound| row_bound.checked_mul(4)?.checked_add(bound))
            .ok_or_else(|| invalid("Codex canonical row bound overflow"))?;
        self.extend_load_reservation(slot, context, canonical_bound)?;
        let entry = codex.canonical_entry(position);
        let mut counted = crate::snapshot_codec::Counter {
            bytes: 0,
            limit: usize::MAX,
        };
        crate::snapshot_codec::write_json(&mut counted, &entry, usize::MAX)
            .expect("canonical Codex row serializes");
        let typed = crate::snapshot_memory::entry_charge(&entry)
            .owned_capacity_bytes
            .checked_mul(2)
            .ok_or_else(|| invalid("Codex row detachment bound overflow"))?;
        let scratch = dom_parse_bound(counted.bytes)
            .and_then(|bound| bound.checked_add(counted.bytes))
            .and_then(|bound| bound.checked_add(typed))
            .ok_or_else(|| invalid("Codex row detachment bound overflow"))?;
        self.extend_load_reservation(slot, context, scratch)?;
        let mut bytes = Vec::with_capacity(counted.bytes);
        crate::snapshot_codec::write_json(&mut bytes, &entry, counted.bytes)
            .expect("counted canonical Codex row serializes");
        assert_eq!(
            (bytes.len(), bytes.capacity()),
            (counted.bytes, counted.bytes)
        );
        drop(entry);
        Ok(sonic_rs::from_slice(&bytes).expect("canonical Codex row deserializes"))
    }

    fn lower_agent_codex_source(
        &self,
        slot: &LoadSlot,
        load: &mut Load,
        events_bound: usize,
        eof: bool,
        context: &Value,
        cancel: &Cancellation,
        deadline: u64,
        usage: &mut [u64; 20],
    ) -> Result<bool, SnapshotError> {
        if !load.agent_codex_prepared {
            let mut lines = 0;
            while load.codex_pending < load.pending.len() && lines < events_bound {
                cancel.check(deadline)?;
                let start = load.codex_pending;
                let newline = memchr::memchr(b'\n', &load.pending[start..]);
                let end = match newline {
                    Some(at) => start + at + 1,
                    None if eof => load.pending.len(),
                    None => break,
                };
                let prior_tail = load
                    .previous
                    .as_ref()
                    .filter(|_| load.codex_changed.is_none())
                    .map_or(0, |previous| {
                        previous.stamp.size.saturating_sub(previous.committed_bytes) as usize
                    });
                let line_bytes = end - start - usize::from(newline.is_some());
                if prior_tail + line_bytes > self.config.entry {
                    return Err(SnapshotError::new(
                        Status::EntryLimit,
                        "source entry exceeds owner bound",
                    ));
                }
                if load.agent_codex.is_none() {
                    self.extend_load_reservation(
                        slot,
                        context,
                        size_of::<crate::codex::CodexReviewContinuation>(),
                    )?;
                    load.agent_codex = Some(Box::default());
                }
                let bound = if end - start == 1 && load.pending[start] == b'\n' {
                    dom_parse_bound(1)
                } else {
                    load.agent_codex
                        .as_ref()
                        .unwrap()
                        .ingest_peak_bound(end - start)
                }
                .ok_or_else(|| invalid("Codex append bound overflow"))?;
                self.extend_load_reservation(slot, context, bound)?;
                #[cfg(test)]
                self.observe_hydration(HydrationObservation::Codex {
                    stamp: slot.stamp,
                    previous: load.previous.is_some(),
                    raw: false,
                    index: true,
                    append: load.previous.is_some(),
                    fallback: None,
                    suffix_bytes: end - start,
                });
                let changed = load
                    .agent_codex
                    .as_mut()
                    .unwrap()
                    .ingest(&load.pending[start..end])?;
                load.codex_changed = Some(
                    load.codex_changed
                        .map_or(changed, |previous| previous.min(changed)),
                );
                usage[2] += (end - start) as u64
                    + if load.pending[start] == b'\n' {
                        0
                    } else {
                        prior_tail as u64
                    };
                usage[3] += 1;
                if newline.is_some() {
                    load.committed = load.pending_start + end as u64;
                }
                if end - start >= 64 {
                    load.fence = load.pending[end - 64..end].to_vec();
                } else {
                    load.fence.extend_from_slice(&load.pending[start..end]);
                    if load.fence.len() > 64 {
                        load.fence.drain(..load.fence.len() - 64);
                    }
                }
                load.codex_pending = end;
                lines += 1;
                let actual = Self::unpublished_load_charge(load);
                self.lock_state().set_load_charge(slot, actual);
            }
            let remaining = &load.pending[load.codex_pending..];
            if !remaining.contains(&b'\n') {
                if remaining.len() > self.config.entry {
                    return Err(SnapshotError::new(
                        Status::EntryLimit,
                        "source entry exceeds owner bound",
                    ));
                }
                let consumed = load.codex_pending;
                load.pending.drain(..consumed);
                load.pending_start += consumed as u64;
                load.codex_pending = 0;
            }
            if !eof || !load.pending.is_empty() || lines > 0 {
                return Ok(false);
            }
            let changed = load.codex_changed.unwrap_or(load.count);
            let keep = load
                .chunks
                .partition_point(|chunk| chunk.start + chunk.entries.len() <= changed);
            let changed = load
                .chunks
                .get(keep)
                .map_or(changed, |chunk| changed.min(chunk.start));
            if changed < load.indexed {
                cancel.check(deadline)?;
                let reservation = load
                    .activity
                    .rewind_reservation_bytes(changed, events_bound);
                self.extend_load_reservation(slot, context, reservation)?;
            }
            load.chunks.truncate(keep);
            load.count = changed;
            if changed < load.indexed {
                let complete = load.activity.rewind_step(changed, events_bound);
                load.activity_from_previous = false;
                load.indexed = load.activity.entry_count();
                usage[6] += 1;
                if !complete {
                    return Ok(false);
                }
            }
            if changed == 0 {
                load.session_id = None;
            }
            load.provisional = load.committed < slot.stamp.size;
            load.agent_codex.get_or_insert_with(Default::default);
            load.agent_codex_prepared = true;
        }
        let codex = load.agent_codex.as_ref().expect("agent Codex continuation");
        let start = load.count;
        let stop = (start + events_bound).min(codex.len());
        if start < stop {
            for position in start..stop {
                cancel.check(deadline)?;
                let chunk = Arc::new(EntryChunk::new(
                    position,
                    vec![self.owned_agent_codex_entry(slot, context, codex, position)?],
                ));
                load.fresh_chunk_bytes +=
                    chunk.charge.owned_capacity_bytes + chunk.charge.opaque_dom_accounted_bytes;
                load.chunks.push(chunk);
                load.count = position + 1;
                self.lock_state()
                    .set_load_charge(slot, Self::unpublished_load_charge(load));
            }
        }
        if load.count < codex.len() {
            return Ok(false);
        }
        if load.session_id.is_none() {
            load.session_id = load
                .chunks
                .iter()
                .flat_map(|chunk| chunk.entries.iter())
                .find_map(|entry| entry.meta().map(|meta| meta.session_id.clone()));
        }
        load.codex_raw = None;
        load.codex_append = None;
        load.codex_append_bytes = 0;
        Ok(true)
    }

    fn lower_codex_source(
        &self,
        slot: &LoadSlot,
        load: &mut Load,
        lowering_events: usize,
        usage: &mut [u64; 20],
    ) -> Result<(), SnapshotError> {
        let suffix = &load.pending;
        let append = load.previous.as_ref().is_some_and(|previous| {
            previous.provider == Provider::Codex
                && load.codex_raw.is_some()
                && load.codex_append.is_some()
        });
        let mut raw = if append {
            load.codex_raw
                .as_ref()
                .expect("cached codex source")
                .as_ref()
                .clone()
        } else {
            Vec::new()
        };
        raw.extend_from_slice(suffix);
        if raw.len() != slot.stamp.size as usize {
            return Err(SnapshotError::new(
                Status::Changed,
                "codex source length changed during lowering",
            ));
        }
        let mut parsed_suffix = append.then(|| parse_codex_bytes(suffix));
        if let (Some(previous), Some(parsed)) = (&load.codex_append, &mut parsed_suffix) {
            for entry in &mut parsed.entries {
                entry.line_index += previous.lines;
            }
        }
        let fallback = match (&load.codex_append, &parsed_suffix) {
            (Some(previous), Some(parsed)) => previous.fallback_reason(parsed),
            _ => Some("cold"),
        };
        #[cfg(test)]
        self.observe_hydration(HydrationObservation::Codex {
            stamp: slot.stamp,
            previous: load.previous.is_some(),
            raw: load.codex_raw.is_some(),
            index: load.codex_append.is_some(),
            append,
            fallback,
            suffix_bytes: suffix.len(),
        });
        let (entries, arenas, index) = if fallback.is_some() {
            let mut start = 0;
            for end in memchr::memchr_iter(b'\n', &raw) {
                if end - start > self.config.entry {
                    return Err(SnapshotError::new(
                        Status::EntryLimit,
                        "source entry exceeds owner bound",
                    ));
                }
                start = end + 1;
            }
            if raw.len() - start > self.config.entry {
                return Err(SnapshotError::new(
                    Status::EntryLimit,
                    "source entry exceeds owner bound",
                ));
            }
            let source_events =
                memchr::memchr_iter(b'\n', &raw).count() + usize::from(!raw.ends_with(b"\n"));
            if source_events > lowering_events {
                return Err(SnapshotError::new(
                    Status::EntryLimit,
                    "nonincremental provider lowering exceeds event work bound",
                ));
            }
            usage[15] += 1;
            usage[16] += raw.len() as u64;
            usage[2] += raw.len() as u64;
            let session = parse_codex_bytes(&raw);
            let entries = lower(&session).entries;
            if entries.len() > lowering_events {
                return Err(SnapshotError::new(
                    Status::EntryLimit,
                    "lowered provider entries exceed work bound",
                ));
            }
            load.chunks.clear();
            load.fresh_chunk_bytes = 0;
            load.activity = ActivityIndex::default();
            load.activity_from_previous = false;
            load.indexed = 0;
            load.session_id = None;
            let arenas = session_arenas(&session, &entries);
            (
                entries,
                arenas,
                CodexAppendIndex::from_session(&session, &raw),
            )
        } else {
            let parsed = parsed_suffix.as_ref().expect("parsed codex suffix");
            if parsed.entries.len() > lowering_events {
                return Err(SnapshotError::new(
                    Status::EntryLimit,
                    "lowered provider entries exceed work bound",
                ));
            }
            usage[2] += suffix.len() as u64;
            let previous = load.codex_append.as_ref().expect("codex append index");
            let entries = previous.lower_suffix(parsed);
            let arenas = session_arenas(parsed, &entries);
            (entries, arenas, previous.append(parsed, suffix))
        };
        usage[3] += entries.len() as u64;
        if load.session_id.is_none() {
            load.session_id = entries
                .iter()
                .find_map(|entry| entry.meta().map(|meta| meta.session_id.clone()));
        }
        let start = load
            .chunks
            .last()
            .map_or(0, |chunk| chunk.start + chunk.entries.len());
        if !entries.is_empty() {
            let chunk = Arc::new(EntryChunk::retaining(start, entries, arenas));
            load.fresh_chunk_bytes +=
                chunk.charge.owned_capacity_bytes + chunk.charge.opaque_dom_accounted_bytes;
            load.chunks.push(chunk);
        }
        load.count = load.chunks.iter().map(|chunk| chunk.entries.len()).sum();
        let committed = raw
            .iter()
            .rposition(|byte| *byte == b'\n')
            .map_or(0, |at| at + 1);
        load.committed = committed as u64;
        load.provisional = committed < raw.len();
        load.fence = raw[committed.saturating_sub(64)..committed].to_vec();
        let cached = raw.len() <= 64 * 1024 * 1024;
        load.codex_append = cached.then(|| Arc::new(index));
        load.codex_append_bytes = load
            .codex_append
            .as_ref()
            .map_or(0, |index| index.accounted_bytes());
        load.codex_raw = cached.then(|| Arc::new(raw));
        load.pending.clear();
        Ok(())
    }
}

#[cfg(test)]
mod codex_append_tests {
    use super::*;
    use crate::gateway::parse_transcript_bytes;

    struct Source {
        directory: PathBuf,
        path: PathBuf,
    }

    impl Source {
        fn new(contents: &str) -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let directory = std::env::temp_dir().join(format!(
                "cc-codex-append-{}-{}-{}",
                std::process::id(),
                now_ms(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&directory).unwrap();
            let path = directory.join("rollout.jsonl");
            std::fs::write(&path, contents).unwrap();
            Self { directory, path }
        }

        fn append(&self, text: &str) {
            std::fs::OpenOptions::new()
                .append(true)
                .open(&self.path)
                .unwrap()
                .write_all(text.as_bytes())
                .unwrap();
        }
    }

    impl Drop for Source {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.directory).unwrap();
        }
    }

    fn context() -> Value {
        json!({"claimant":"append-test","admission":"hook","authority":{"kind":"user","effective_uid":unsafe { libc::geteuid() }.to_string()},"registry_generation":crate::toolcall::ToolRegistrySnapshot::from_specs(HashMap::new()).fingerprint()})
    }

    fn store() -> NativeStore {
        NativeStore::new_agent(&json!({"max_read_bytes_per_step":2*1024*1024,"max_retained_bytes":256*1024*1024,"max_entry_bytes":2*1024*1024,"max_leases":64,"reserved_hook_leases":1,"reserved_hook_accounted_bytes":4096})).unwrap()
    }

    fn request(store: &NativeStore, request: &Value, context: &Value) -> Value {
        if store.agent_sources {
            store.request_agent(request, context, &Cancellation::default())
        } else {
            store.request(request, context, &Cancellation::default())
        }
    }

    fn acquire(store: &NativeStore, source: &Source, context: &Value) -> Value {
        let first = json!({"schema":SCHEMA,"id":"codex-append","operation":"acquire","path":source.path.to_string_lossy().as_ref(),"classifier":{"id":"native","version":"1"},"deadline_unix_ms":now_ms()+30_000,"limits":{"max_read_bytes":256*1024*1024,"max_source_read_bytes":256*1024*1024,"max_events":200000,"max_items":256,"max_output_bytes":1024*1024,"max_discovery_entries":1000,"max_sources":100}});
        let mut response = request(store, &first, context);
        for _ in 0..256 {
            if response["status"].as_str() != Some("incomplete") {
                return response;
            }
            response = request(
                store,
                &json!({"schema":SCHEMA,"id":"codex-resume","operation":"resume","cursor":response["cursor"]}),
                context,
            );
        }
        panic!("codex source did not complete: {response:?}");
    }

    fn parity(store: &NativeStore, response: &Value, source: &Source, context: &Value) {
        assert_eq!(response["status"].as_str(), Some("ok"), "{response:?}");
        let snapshot = store
            .pin(&response["data"]["description"]["handle"], context)
            .unwrap();
        let raw = std::fs::read(&source.path).unwrap();
        let expected = parse_transcript_bytes(&raw).unwrap();
        assert_eq!(
            sonic_rs::to_string(&snapshot.entries()).unwrap(),
            sonic_rs::to_string(&expected.entries).unwrap()
        );
    }

    #[test]
    fn twelve_codex_appends_lower_only_the_suffix() {
        let initial = format!(
            "{{\"timestamp\":\"2026-01-02T03:04:05Z\",\"type\":\"session_meta\",\"payload\":{{\"id\":\"thread-append\",\"cwd\":\"/tmp\",\"originator\":\"codex_exec\",\"source\":\"exec\"}}}}\n{{\"timestamp\":\"2026-01-02T03:04:06Z\",\"type\":\"event_msg\",\"payload\":{{\"type\":\"user_message\",\"message\":\"{}\"}}}}\n",
            "x".repeat(1024 * 1024)
        );
        let source = Source::new(&initial);
        let store = store();
        let context = context();
        let first = acquire(&store, &source, &context);
        assert_eq!(first["status"].as_str(), Some("ok"), "{first:?}");
        let before = store.state.lock().unwrap().counters;
        let mut appended_bytes = 0u64;
        let mut final_response = Value::new_null();
        for index in 0..12 {
            let line = format!(
                "{{\"timestamp\":\"2026-01-02T03:04:07Z\",\"type\":\"event_msg\",\"payload\":{{\"type\":\"user_message\",\"message\":\"new-{index}\"}}}}\n"
            );
            appended_bytes += line.len() as u64;
            source.append(&line);
            final_response = acquire(&store, &source, &context);
            assert_eq!(
                final_response["status"].as_str(),
                Some("ok"),
                "{final_response:?}"
            );
        }
        let after = store.state.lock().unwrap().counters;
        assert_eq!(after[15] - before[15], 0);
        assert_eq!(after[16] - before[16], 0);
        assert_eq!(after[3] - before[3], 12);
        assert!(after[1] - before[1] >= appended_bytes);
        assert!(after[1] - before[1] <= appended_bytes + 12 * 256);
        assert!(after[2] - before[2] <= appended_bytes);
        parity(&store, &final_response, &source, &context);
    }

    #[test]
    fn appended_response_item_retroactively_suppresses_an_event_echo() {
        let source = Source::new(concat!(
            "{\"timestamp\":\"2026-01-02T03:04:05Z\",\"type\":\"session_meta\",\"payload\":{\"id\":\"thread-echo\"}}\n",
            "{\"timestamp\":\"2026-01-02T03:04:06Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"user_message\",\"message\":\"echo\"}}\n"
        ));
        let store = store();
        let context = context();
        let first = acquire(&store, &source, &context);
        assert_eq!(first["status"].as_str(), Some("ok"), "{first:?}");
        let before = store.state.lock().unwrap().counters[16];
        source.append("{\"timestamp\":\"2026-01-02T03:04:07Z\",\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":[{\"type\":\"input_text\",\"text\":\"echo\"}]}}\n");
        let second = acquire(&store, &source, &context);
        parity(&store, &second, &source, &context);
        assert_eq!(store.state.lock().unwrap().counters[16], before);
    }

    #[test]
    fn first_appended_session_meta_updates_prior_thread_identity() {
        let source = Source::new("{\"timestamp\":\"2026-01-02T03:04:05Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"user_message\",\"message\":\"early\"}}\n");
        let store = store();
        let context = context();
        let first = acquire(&store, &source, &context);
        assert_eq!(first["status"].as_str(), Some("ok"), "{first:?}");
        let before = store.state.lock().unwrap().counters[16];
        source.append("{\"timestamp\":\"2026-01-02T03:04:06Z\",\"type\":\"session_meta\",\"payload\":{\"id\":\"thread-late\",\"cwd\":\"/tmp\"}}\n");
        let second = acquire(&store, &source, &context);
        parity(&store, &second, &source, &context);
        assert_eq!(store.state.lock().unwrap().counters[16], before);
    }

    #[test]
    fn first_model_and_partial_line_append_match_full_lowering() {
        let source = Source::new(concat!(
            "{\"timestamp\":\"2026-01-02T03:04:05Z\",\"type\":\"session_meta\",\"payload\":{\"id\":\"thread-model\"}}\n",
            "{\"timestamp\":\"2026-01-02T03:04:06Z\",\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"text\":\"hello\"}]}}\n"
        ));
        let store = store();
        let context = context();
        let first = acquire(&store, &source, &context);
        assert_eq!(first["status"].as_str(), Some("ok"), "{first:?}");
        source.append("{\"timestamp\":\"2026-01-02T03:04:07Z\",\"type\":\"turn_context\",\"payload\":{\"model\":\"gpt-6\"}}\n");
        let second = acquire(&store, &source, &context);
        parity(&store, &second, &source, &context);
        source.append("{\"timestamp\":\"2026-01-02T03:04:08Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"user_message\",\"message\":\"par");
        let partial = acquire(&store, &source, &context);
        parity(&store, &partial, &source, &context);
        source.append("tial\"}}\n");
        let completed = acquire(&store, &source, &context);
        parity(&store, &completed, &source, &context);
    }

    #[test]
    fn cross_revision_pairing_malformed_lines_and_truncation_match_full_lowering() {
        let source = Source::new(concat!(
            "{\"timestamp\":\"2026-01-02T03:04:05Z\",\"type\":\"session_meta\",\"payload\":{\"id\":\"thread-pair\"}}\n",
            "{\"timestamp\":\"2026-01-02T03:04:06Z\",\"type\":\"inter_agent_communication_metadata\",\"payload\":{\"trigger_turn\":true}}\n"
        ));
        let store = store();
        let context = context();
        let first = acquire(&store, &source, &context);
        assert_eq!(first["status"].as_str(), Some("ok"), "{first:?}");
        let prior_lowered = store.state.lock().unwrap().counters[16];
        let suffixes = [
            "{\"timestamp\":\"2026-01-02T03:04:07Z\",\"type\":\"response_item\",\"payload\":{\"type\":\"agent_message\",\"content\":[{\"text\":\"delegate\"}]}}\n",
            "{\"timestamp\":\"2026-01-02T03:04:08Z\",\"type\":\"response_item\",\"payload\":{\"type\":\"function_call\",\"name\":\"exec_command\",\"call_id\":\"call-1\",\"arguments\":\"{\\\"cmd\\\":\\\"pwd\\\"}\"}}\n",
            "{\"timestamp\":\"2026-01-02T03:04:09Z\",\"type\":\"response_item\",\"payload\":{\"type\":\"function_call_output\",\"call_id\":\"call-1\",\"output\":\"done\"}}\n",
            "{\"type\":broken-json}\n",
        ];
        for suffix in suffixes {
            source.append(suffix);
            let response = acquire(&store, &source, &context);
            parity(&store, &response, &source, &context);
        }
        assert_eq!(store.state.lock().unwrap().counters[16], prior_lowered);
        std::fs::write(
            &source.path,
            "{\"timestamp\":\"2026-01-02T03:04:10Z\",\"type\":\"session_meta\",\"payload\":{\"id\":\"thread-new\"}}\n",
        )
        .unwrap();
        let truncated = acquire(&store, &source, &context);
        parity(&store, &truncated, &source, &context);
        assert_eq!(store.state.lock().unwrap().counters[16], prior_lowered);
    }
    fn process_cpu_ns() -> u64 {
        let mut value = std::mem::MaybeUninit::<libc::timespec>::uninit();
        assert_eq!(
            unsafe { libc::clock_gettime(libc::CLOCK_PROCESS_CPUTIME_ID, value.as_mut_ptr()) },
            0
        );
        let value = unsafe { value.assume_init() };
        value.tv_sec as u64 * 1_000_000_000 + value.tv_nsec as u64
    }

    #[test]
    fn agent_codex_raw_append_counts_actual_work_at_32_and_10000_rows() {
        for count in [32, 10_000] {
            let mut initial = String::from("{\"timestamp\":\"2026-01-02T03:04:05Z\",\"type\":\"session_meta\",\"payload\":{\"id\":\"raw-cost\"}}\n");
            for index in 0..count {
                initial.push_str(&format!("{{\"timestamp\":\"2026-01-02T03:04:06Z\",\"type\":\"event_msg\",\"payload\":{{\"type\":\"user_message\",\"message\":\"row-{index}\"}}}}\n"));
            }
            let source = Source::new(&initial);
            let store = store();
            let context = context();
            let first = acquire(&store, &source, &context);
            assert_eq!(first["status"].as_str(), Some("ok"), "{first:?}");
            let old = store
                .pin(&first["data"]["description"]["handle"], &context)
                .unwrap();
            let old_rows = sonic_rs::to_string(&old.entries()).unwrap();
            assert!(old.codex_raw.is_none());
            let continuation = {
                let state = store.lock_state();
                state
                    .generations
                    .get(&snapshot_key(&old))
                    .unwrap()
                    .agent_codex
                    .as_ref()
                    .unwrap()
                    .as_ref() as *const crate::codex::CodexReviewContinuation
                    as usize
            };
            let before = store.lock_state().counters;
            let owner_work = store.retained_work.load(Ordering::Relaxed);
            let cpu_before = process_cpu_ns();
            let started = std::time::Instant::now();
            let suffix = "{\"timestamp\":\"2026-01-02T03:04:07Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"user_message\",\"message\":\"fresh\"}}\n";
            source.append(suffix);
            let next = acquire(&store, &source, &context);
            let elapsed = started.elapsed().as_nanos() as u64;
            let cpu_ns = process_cpu_ns() - cpu_before;
            assert_eq!(next["status"].as_str(), Some("ok"), "{next:?}");
            let current = store
                .pin(&next["data"]["description"]["handle"], &context)
                .unwrap();
            let after = store.lock_state().counters;
            assert_eq!(after[2] - before[2], suffix.len() as u64);
            assert_eq!(after[3] - before[3], 1);
            assert_eq!(after[15] - before[15], 0);
            assert_eq!(after[16] - before[16], 0);
            assert!(after[1] - before[1] <= suffix.len() as u64 + 192);
            assert!(current.codex_raw.is_none());
            {
                let state = store.lock_state();
                let old_record = state.generations.get(&snapshot_key(&old)).unwrap();
                assert!(old_record.agent_codex.is_none());
                let next_record = state.generations.get(&snapshot_key(&current)).unwrap();
                assert_eq!(
                    next_record.agent_codex.as_ref().unwrap().as_ref()
                        as *const crate::codex::CodexReviewContinuation
                        as usize,
                    continuation
                );
            }
            assert_eq!(sonic_rs::to_string(&old.entries()).unwrap(), old_rows);
            eprintln!(
                "agent_raw_append_probe {}",
                json!({"rows":count,"source_read_bytes":after[1]-before[1],"decoded_bytes":after[2]-before[2],"canonical_rows":after[3]-before[3],"nonincremental_source_bytes":after[16]-before[16],"owner_work":store.retained_work.load(Ordering::Relaxed)-owner_work,"elapsed_ns":elapsed,"process_cpu_ns":cpu_ns})
            );
            parity(&store, &next, &source, &context);
        }
    }

    #[test]
    fn generic_acquire_rebuilds_an_interior_rewrite_followed_by_append() {
        let suffix = "{\"timestamp\":\"2026-01-02T03:04:06Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"user_message\",\"message\":\"unchanged boundary text longer than sixty-four bytes 012345678901234567890123456789\"}}\n";
        let initial = format!("{{\"timestamp\":\"2026-01-02T03:04:05Z\",\"type\":\"session_meta\",\"payload\":{{\"id\":\"before\"}}}}\n{suffix}");
        let source = Source::new(&initial);
        let store = NativeStore::new(&json!({"reserved_hook_accounted_bytes":4096})).unwrap();
        let context = context();
        let first = acquire(&store, &source, &context);
        assert_eq!(first["status"].as_str(), Some("ok"), "{first:?}");
        let before = store.lock_state().counters;
        std::fs::write(&source.path, initial.replace("before", "after!")).unwrap();
        source.append(suffix);
        let next = acquire(&store, &source, &context);
        parity(&store, &next, &source, &context);
        assert_eq!(store.lock_state().counters[4] - before[4], 1);
        let snapshot = store
            .pin(&next["data"]["description"]["handle"], &context)
            .unwrap();
        assert_eq!(snapshot.session_id, "after!");
    }

    #[test]
    fn agent_codex_continuation_remains_incremental_above_the_old_raw_cache_cap() {
        let mut initial = String::from("{\"timestamp\":\"2026-01-02T03:04:05Z\",\"type\":\"session_meta\",\"payload\":{\"id\":\"large-agent\"}}\n");
        let message = "x".repeat(64 * 1024);
        for _ in 0..1025 {
            initial.push_str(&format!("{{\"timestamp\":\"2026-01-02T03:04:06Z\",\"type\":\"event_msg\",\"payload\":{{\"type\":\"user_message\",\"message\":\"{message}\"}}}}\n"));
        }
        assert!(initial.len() > 64 * 1024 * 1024);
        let source = Source::new(&initial);
        drop(initial);
        let store = NativeStore::new_agent(&json!({"max_retained_bytes":1024*1024*1024,"max_read_bytes_per_step":2*1024*1024,"max_entry_bytes":2*1024*1024,"reserved_hook_accounted_bytes":4096})).unwrap();
        let context = context();
        let first = acquire(&store, &source, &context);
        assert_eq!(first["status"].as_str(), Some("ok"), "{first:?}");
        let before = store.lock_state().counters;
        let suffix = "{\"timestamp\":\"2026-01-02T03:04:07Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"user_message\",\"message\":\"large-fresh\"}}\n";
        source.append(suffix);
        let next = acquire(&store, &source, &context);
        assert_eq!(next["status"].as_str(), Some("ok"), "{next:?}");
        let after = store.lock_state().counters;
        assert_eq!(after[2] - before[2], suffix.len() as u64);
        assert_eq!(after[3] - before[3], 1);
        assert_eq!(after[15] - before[15], 0);
        assert_eq!(after[16] - before[16], 0);
        assert!(after[1] - before[1] <= suffix.len() as u64 + 192);
        let snapshot = store
            .pin(&next["data"]["description"]["handle"], &context)
            .unwrap();
        assert!(snapshot.codex_raw.is_none());
        assert!(store
            .lock_state()
            .generations
            .get(&snapshot_key(&snapshot))
            .unwrap()
            .agent_codex
            .is_some());
    }

    #[test]
    fn agent_raw_owned_values_preserve_canonical_payloads_and_old_leases() {
        let source = Source::new(concat!(
            "{\"timestamp\":\"2026-01-02T03:04:05Z\",\"type\":\"session_meta\",\"payload\":{\"id\":\"owned\",\"extra\":{\"n\":1.2300e+12}}}\n",
            "{\"timestamp\":\"2026-01-02T03:04:06Z\",\"type\":\"response_item\",\"payload\":{\"type\":\"function_call\",\"name\":\"exec_command\",\"call_id\":\"call-owned\",\"arguments\":\"{\\\"cmd\\\":\\\"printf payload\\\",\\\"extra\\\":{\\\"n\\\":12345678901234567890}}\"}}\n",
            "{\"timestamp\":\"2026-01-02T03:04:07Z\",\"type\":\"response_item\",\"payload\":{\"type\":\"function_call_output\",\"call_id\":\"call-owned\",\"output\":{\"metadata\":{\"exit_code\":0},\"output\":[\"payload\",{\"n\":1.2300e+12}]}}}\n",
            "{\"timestamp\":\"2026-01-02T03:04:08Z\",\"type\":\"unrecognized_event\",\"payload\":{\"raw\":[{\"nested\":\"verbatim\"}],\"n\":1.2300e+12}}\n"
        ));
        let store = store();
        let context = context();
        let first = acquire(&store, &source, &context);
        parity(&store, &first, &source, &context);
        let prior = store
            .pin(&first["data"]["description"]["handle"], &context)
            .unwrap();
        let prior_rows = sonic_rs::to_string(&prior.entries()).unwrap();
        source.append("{\"timestamp\":\"2026-01-02T03:04:09Z\",\"type\":\"turn_context\",\"payload\":{\"model\":\"gpt-6\"}}\n");
        let next = acquire(&store, &source, &context);
        parity(&store, &next, &source, &context);
        assert_eq!(sonic_rs::to_string(&prior.entries()).unwrap(), prior_rows);
        assert!(prior
            .chunks
            .iter()
            .all(|chunk| chunk.entries.arenas().is_empty()));
        store.assert_conserved();
    }

    #[test]
    fn agent_raw_owned_escaped_strings_preserve_canonical_payloads() {
        let message = "\0".repeat(65_536);
        let source = Source::new(&format!(
            "{}\n{}\n",
            json!({"timestamp":"2026-01-02T03:04:05Z","type":"session_meta","payload":{"id":"escaped"}}),
            json!({"timestamp":"2026-01-02T03:04:06Z","type":"event_msg","payload":{"type":"user_message","message":message}}),
        ));
        let store = store();
        let context = context();
        let response = acquire(&store, &source, &context);
        parity(&store, &response, &source, &context);
        let snapshot = store
            .pin(&response["data"]["description"]["handle"], &context)
            .unwrap();
        assert!(snapshot
            .chunks
            .iter()
            .all(|chunk| chunk.entries.arenas().is_empty()));
        store.assert_conserved();
    }

    #[test]
    fn agent_raw_ingestion_respects_the_cumulative_event_budget() {
        let source = Source::new(concat!(
            "{\"timestamp\":\"2026-01-02T03:04:05Z\",\"type\":\"session_meta\",\"payload\":{\"id\":\"budget\"}}\n",
            "{\"timestamp\":\"2026-01-02T03:04:06Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"user_message\",\"message\":\"budgeted\"}}\n"
        ));
        let store = store();
        let context = context();
        let first = json!({"schema":SCHEMA,"id":"agent-budget","operation":"acquire","path":source.path.to_string_lossy().as_ref(),"classifier":{"id":"native","version":"1"},"deadline_unix_ms":now_ms()+30_000,"limits":{"max_read_bytes":1024*1024,"max_source_read_bytes":1024*1024,"max_events":1,"max_items":256,"max_output_bytes":1024*1024,"max_discovery_entries":1000,"max_sources":100}});
        let mut response = request(&store, &first, &context);
        for _ in 0..16 {
            if response["status"].as_str() != Some("incomplete") {
                break;
            }
            response = request(
                &store,
                &json!({"schema":SCHEMA,"id":"agent-budget-resume","operation":"resume","cursor":response["cursor"]}),
                &context,
            );
        }
        assert_eq!(
            response["status"].as_str(),
            Some("source_limit"),
            "{response:?}"
        );
        assert_eq!(store.lock_state().counters[3], 1);
        assert!(store.lock_state().latest.is_empty());
    }

    #[test]
    fn generic_unchanged_stamp_reuses_its_exact_cached_snapshot() {
        let source = Source::new("{\"timestamp\":\"2026-01-02T03:04:05Z\",\"type\":\"session_meta\",\"payload\":{\"id\":\"unchanged\"}}\n");
        let store = NativeStore::new(&json!({"reserved_hook_accounted_bytes":4096})).unwrap();
        let context = context();
        let first = acquire(&store, &source, &context);
        assert_eq!(first["status"].as_str(), Some("ok"), "{first:?}");
        let before = store.lock_state().counters;
        let same = acquire(&store, &source, &context);
        assert_eq!(same["status"].as_str(), Some("ok"), "{same:?}");
        let after = store.lock_state().counters;
        assert_eq!(after[1], before[1]);
        assert_eq!(after[2], before[2]);
        assert_eq!(after[4], before[4]);
        assert_eq!(
            same["data"]["description"]["handle"]["snapshot_id"],
            first["data"]["description"]["handle"]["snapshot_id"]
        );
    }

    #[test]
    fn agent_codex_sealing_an_existing_partial_record_does_not_decode_it_again() {
        let source = Source::new(concat!(
            "{\"timestamp\":\"2026-01-02T03:04:05Z\",\"type\":\"session_meta\",\"payload\":{\"id\":\"seal\"}}\n",
            "{\"timestamp\":\"2026-01-02T03:04:06Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"user_message\",\"message\":\"sealed\"}}"
        ));
        let store = store();
        let context = context();
        let first = acquire(&store, &source, &context);
        parity(&store, &first, &source, &context);
        let before = store.lock_state().counters;
        source.append("\n");
        let next = acquire(&store, &source, &context);
        parity(&store, &next, &source, &context);
        let after = store.lock_state().counters;
        assert_eq!(after[2] - before[2], 1);
        assert_eq!(after[16] - before[16], 0);
        let snapshot = store
            .pin(&next["data"]["description"]["handle"], &context)
            .unwrap();
        assert!(!snapshot.provisional_tail);
        assert_eq!(snapshot.committed_bytes, snapshot.stamp.size);
    }

    #[test]
    fn generic_acquire_detects_mutation_during_a_pinned_load() {
        let source = Source::new(concat!(
            "{\"timestamp\":\"2026-01-02T03:04:05Z\",\"type\":\"session_meta\",\"payload\":{\"id\":\"pinned\"}}\n",
            "{\"timestamp\":\"2026-01-02T03:04:06Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"user_message\",\"message\":\"body\"}}\n"
        ));
        let store = NativeStore::new(&json!({"reserved_hook_accounted_bytes":4096})).unwrap();
        let context = context();
        let path = source.path.clone();
        *store.read_hook.lock().unwrap() = Some(Arc::new(move || {
            std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap()
                .write_all(b"\n")
                .unwrap();
        }));
        let response = acquire(&store, &source, &context);
        assert_eq!(response["status"].as_str(), Some("changed"), "{response:?}");
        assert!(store.lock_state().latest.is_empty());
    }

    #[test]
    fn agent_raw_echo_rewind_is_admitted_before_mutating_the_prefix() {
        let source = Source::new(concat!(
            "{\"timestamp\":\"2026-01-02T03:04:05Z\",\"type\":\"session_meta\",\"payload\":{\"id\":\"rewind-admission\"}}\n",
            "{\"timestamp\":\"2026-01-02T03:04:06Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"user_message\",\"message\":\"echo\"}}\n"
        ));
        let mut store = store();
        let context = context();
        let first = acquire(&store, &source, &context);
        let prior = store
            .pin(&first["data"]["description"]["handle"], &context)
            .unwrap();
        let prior_rows = sonic_rs::to_string(&prior.entries()).unwrap();
        source.append("{\"timestamp\":\"2026-01-02T03:04:07Z\",\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":[{\"type\":\"input_text\",\"text\":\"echo\"}]}}\n");
        let mut response = request(
            &store,
            &json!({"schema":SCHEMA,"id":"rewind-admission","operation":"acquire","path":source.path.to_string_lossy().as_ref(),"classifier":{"id":"native","version":"1"},"deadline_unix_ms":now_ms()+30_000,"limits":{"max_read_bytes":1024*1024,"max_source_read_bytes":1024*1024,"max_events":100,"max_items":256,"max_output_bytes":1024*1024,"max_discovery_entries":1000,"max_sources":100}}),
            &context,
        );
        let mut pending_slot = None;
        for _ in 0..16 {
            assert_eq!(
                response["status"].as_str(),
                Some("incomplete"),
                "{response:?}"
            );
            let slot = store
                .lock_state()
                .waiters
                .values()
                .next()
                .expect("pending raw source")
                .load
                .clone();
            let ready = {
                let load = slot.work.lock().unwrap();
                load.offset == slot.stamp.size
                    && load.pending.is_empty()
                    && !load.agent_codex_prepared
                    && load
                        .codex_changed
                        .is_some_and(|changed| changed < load.indexed)
            };
            if ready {
                pending_slot = Some(slot);
                break;
            }
            response = request(
                &store,
                &json!({"schema":SCHEMA,"id":"rewind-admission-resume","operation":"resume","cursor":response["cursor"]}),
                &context,
            );
        }
        let slot = pending_slot.expect("raw echo reached the pre-rewind boundary");
        let cap = store.config.retained;
        store.config.retained = 0;
        {
            let mut load = slot.work.lock().unwrap();
            let entries = load.activity.entry_count();
            let copies = load.activity.copied_work();
            let chunks = load.chunks.len();
            let count = load.count;
            let mut usage = [0; 20];
            let error = store
                .lower_agent_codex_source(
                    &slot,
                    &mut load,
                    1,
                    true,
                    &context,
                    &Cancellation::default(),
                    now_ms() + 30_000,
                    &mut usage,
                )
                .unwrap_err();
            assert_eq!(error.status, Status::RetainedLimit);
            assert_eq!(load.activity.entry_count(), entries);
            assert_eq!(load.activity.copied_work(), copies);
            assert_eq!(load.chunks.len(), chunks);
            assert_eq!(load.count, count);
        }
        store.config.retained = cap;
        let released = request(
            &store,
            &json!({"schema":SCHEMA,"id":"rewind-release","operation":"release","kind":"cursor","token":response["cursor"]}),
            &context,
        );
        assert_eq!(released["status"].as_str(), Some("ok"), "{released:?}");
        assert!(store.lock_state().waiters.is_empty());
        assert_eq!(sonic_rs::to_string(&prior.entries()).unwrap(), prior_rows);
        store.assert_conserved();
    }

    #[test]
    fn agent_codex_diag_echo_reservations() {
        let mut text = String::from("{\"timestamp\":\"2026-01-02T03:04:05Z\",\"type\":\"session_meta\",\"payload\":{\"id\":\"activity-prefix\"}}\n");
        for index in 0..10_000 {
            text.push_str(&format!("{{\"timestamp\":\"2026-01-02T03:04:06Z\",\"type\":\"event_msg\",\"payload\":{{\"type\":\"user_message\",\"message\":\"old-{index}\"}}}}\n"));
        }
        let source = Source::new(&text);
        let store = store();
        let context = context();
        let stats = |label: &str| {
            let response = request(
                &store,
                &json!({"schema":SCHEMA,"id":"stats","operation":"stats"}),
                &context,
            );
            eprintln!("diag_gauges {label} {}", response["data"]["gauges"]);
        };
        let initial = acquire(&store, &source, &context);
        stats("initial");
        let mut prior = store
            .pin(&initial["data"]["description"]["handle"], &context)
            .unwrap();
        let fresh = "{\"timestamp\":\"2026-01-02T03:04:07Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"user_message\",\"message\":\"new-fresh\"}}\n";
        let echo = "{\"timestamp\":\"2026-01-02T03:04:08Z\",\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":[{\"type\":\"input_text\",\"text\":\"new-fresh\"}]}}\n";
        let interlude = "{\"timestamp\":\"2026-01-02T03:04:07Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"agent_message\",\"message\":\"interlude\"}}\n";
        for (phase, suffix) in [("fresh", fresh), ("interlude", interlude), ("echo", echo)] {
            let activity = &prior.activity;
            let entries = activity.entry_count();
            eprintln!(
                "diag_activity {phase} {}",
                json!({"entries":entries,"accounted":activity.accounted_bytes(),
                    "rewind":activity.rewind_reservation_bytes(entries.saturating_sub(2), 4096),
                    "container":activity.append_container_reservation_bytes(3, 0, 0),
                    "keys":activity.append_key_reservation_bytes(3, 0, 64, 0),
                    "undo":activity.undo_reservation_bytes(3, 0),
                    "pending_allocation":activity.pending_allocation_reservation()})
            );
            source.append(suffix);
            let next = acquire(&store, &source, &context);
            stats(phase);
            eprintln!("diag_reply {phase} {} {}", next["status"], next["reason"]);
            if next["status"].as_str() != Some("ok") {
                break;
            }
            prior = store
                .pin(&next["data"]["description"]["handle"], &context)
                .unwrap();
        }
    }

    #[test]
    fn agent_raw_fresh_and_echo_append_preserve_the_activity_prefix() {
        for count in [32, 10_000] {
            let mut text = String::from("{\"timestamp\":\"2026-01-02T03:04:05Z\",\"type\":\"session_meta\",\"payload\":{\"id\":\"activity-prefix\"}}\n");
            for index in 0..count {
                text.push_str(&format!("{{\"timestamp\":\"2026-01-02T03:04:06Z\",\"type\":\"event_msg\",\"payload\":{{\"type\":\"user_message\",\"message\":\"old-{index}\"}}}}\n"));
            }
            let source = Source::new(&text);
            let store = store();
            let context = context();
            let initial = acquire(&store, &source, &context);
            let mut prior = store
                .pin(&initial["data"]["description"]["handle"], &context)
                .unwrap();
            let fresh = "{\"timestamp\":\"2026-01-02T03:04:07Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"user_message\",\"message\":\"new-fresh\"}}\n";
            let echo = "{\"timestamp\":\"2026-01-02T03:04:08Z\",\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":[{\"type\":\"input_text\",\"text\":\"new-fresh\"}]}}\n";
            let interlude = "{\"timestamp\":\"2026-01-02T03:04:07Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"agent_message\",\"message\":\"interlude\"}}\n";
            for (phase, suffix) in [("fresh", fresh), ("interlude", interlude), ("echo", echo)] {
                let old_rows = sonic_rs::to_string(&prior.entries()).unwrap();
                let copied = prior.activity.copied_work();
                let before = store.lock_state().counters;
                source.append(suffix);
                let started = std::time::Instant::now();
                let cpu_before = process_cpu_ns();
                let next = acquire(&store, &source, &context);
                let cpu = process_cpu_ns() - cpu_before;
                let wall = started.elapsed().as_nanos();
                assert_eq!(next["status"].as_str(), Some("ok"), "{next:?}");
                let current = store
                    .pin(&next["data"]["description"]["handle"], &context)
                    .unwrap();
                let after = store.lock_state().counters;
                let work = current.activity.work();
                let copies = current.activity.copied_work().checked_sub(copied).unwrap();
                assert!(
                    work.indexed_entries <= if phase == "echo" { 3 } else { 1 },
                    "{phase} N{count}: {work:?}"
                );
                assert!(copies <= 512, "{phase} N{count}: copied {copies}");
                assert_eq!(after[2] - before[2], suffix.len() as u64);
                assert_eq!(after[3] - before[3], 1);
                assert_eq!(after[15] - before[15], 0);
                assert_eq!(after[16] - before[16], 0);
                assert!(after[1] - before[1] <= suffix.len() as u64 + 192);
                assert_eq!(sonic_rs::to_string(&prior.entries()).unwrap(), old_rows);
                parity(&store, &next, &source, &context);
                let warm_before = store.lock_state().counters;
                let warm = acquire(&store, &source, &context);
                let warm_after = store.lock_state().counters;
                assert_eq!(warm["status"].as_str(), Some("ok"), "{warm:?}");
                for counter in [1, 2, 3, 4, 5, 6, 15, 16] {
                    assert_eq!(warm_before[counter], warm_after[counter]);
                }
                assert_eq!(
                    warm["data"]["description"]["handle"]["generation"],
                    next["data"]["description"]["handle"]["generation"]
                );
                eprintln!(
                    "agent_raw_activity_probe {}",
                    json!({"rows":count,"phase":phase,"indexed_entries":work.indexed_entries,"copied_leaf_values":copies,"source_read_bytes":after[1]-before[1],"decoded_bytes":after[2]-before[2],"wall_ns":wall,"process_cpu_ns":cpu})
                );
                prior = current;
            }
            store.assert_conserved();
        }
    }
}
