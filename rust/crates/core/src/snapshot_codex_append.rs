use crate::codex::lower::{blocks_text, echo_sets, lower_entry};
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
        size_of::<Self>()
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
    fn lower_codex_source(
        &self,
        slot: &LoadSlot,
        load: &mut Load,
        lowering_events: usize,
        usage: &mut [u64; 18],
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
        let (entries, index) = if fallback.is_some() {
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
            load.activity = ActivityIndex::default();
            load.indexed = 0;
            load.session_id = None;
            (entries, CodexAppendIndex::from_session(&session, &raw))
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
            (
                previous.lower_suffix(parsed),
                previous.append(parsed, suffix),
            )
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
            load.chunks.push(Arc::new(EntryChunk::new(start, entries)));
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
        NativeStore::new(&json!({"max_read_bytes_per_step":2*1024*1024,"max_retained_bytes":256*1024*1024,"max_entry_bytes":2*1024*1024,"max_leases":64,"reserved_hook_leases":1,"reserved_hook_accounted_bytes":4096})).unwrap()
    }

    fn acquire(store: &NativeStore, source: &Source, context: &Value) -> Value {
        let request = json!({"schema":SCHEMA,"id":"codex-append","operation":"acquire","path":source.path.to_string_lossy().as_ref(),"classifier":{"id":"native","version":"1"},"deadline_unix_ms":now_ms()+30_000,"limits":{"max_read_bytes":4*1024*1024,"max_events":4096,"max_items":256,"max_output_bytes":1024*1024,"max_discovery_entries":1000,"max_sources":100}});
        let mut response = store.request(&request, context, &Cancellation::default());
        for _ in 0..64 {
            if response["status"].as_str() != Some("incomplete") {
                return response;
            }
            response = store.request(
                &json!({"schema":SCHEMA,"id":"codex-resume","operation":"resume","cursor":response["cursor"]}),
                context,
                &Cancellation::default(),
            );
        }
        panic!("codex source did not complete");
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
        assert!(store.state.lock().unwrap().counters[16] > before);
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
        assert!(store.state.lock().unwrap().counters[16] > before);
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
        assert!(store.state.lock().unwrap().counters[16] > prior_lowered);
    }
}
