use std::collections::{HashMap, VecDeque};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::mem::size_of;
use std::path::Path;

use sonic_rs::json;

use super::{incomplete, GrepEvent, GrepReducer};
use crate::gateway::{sniff_provider, Provider};
use crate::scan::{ScanBudget, ScanControl, StagingReservation};
use crate::scan_checkpoint::{
    FileLayer, GrepCheckpoints, Prefix, QueryLayer, Queued, ReducerState, Replayed, SourceRecord,
    Span, PREFIX_SEGMENT,
};
use crate::scan_stream::{LineSpan, SourceStream};
use crate::snapshot::{Cancellation, SnapshotError, SourceStamp, Status};
use crate::snapshot_memory::entry_charge;
use crate::types::{ContentBlock, Entry};

const FENCE_BYTES: usize = 64;

struct ToolName {
    name: String,
    referenced: bool,
}

struct StreamSlot<'store> {
    index: usize,
    line: LineSpan,
    digest: u64,
    entry: Entry,
    charge: usize,
    pattern_ids: Option<Vec<usize>>,
    staging: StagingReservation<'store>,
}

enum Validity {
    Valid,
    Invalid,
    Unverified,
}

struct GrepStream<'store> {
    query_key: Option<String>,
    size: u64,
    revision: String,
    generation: String,
    source_bytes: usize,
    render_names: bool,
    names: HashMap<String, ToolName>,
    staging: StagingReservation<'store>,
    queue: VecDeque<StreamSlot<'store>>,
    history: Vec<Replayed>,
    parsed: usize,
    decided: usize,
    emitted: usize,
    last_emitted: Option<usize>,
    last_hit: Option<usize>,
    stopped: Option<usize>,
    committed: u64,
    fence: Vec<u8>,
    sniffed: bool,
    names_base: u64,
    names_final: bool,
    capture: Option<(FileLayer, QueryLayer)>,
    poisoned: bool,
    prefix: Prefix,
    proof: Option<Prefix>,
    frozen: bool,
    tail: Option<(LineSpan, u64)>,
}

fn unresolved(names: &HashMap<String, ToolName>, entry: &Entry) -> bool {
    entry
        .tool_results()
        .any(|result| !names.contains_key(result.tool_use_id.as_str()))
}

fn resolve<'n>(
    names: &'n mut HashMap<String, ToolName>,
    entry: &Entry,
    mark: bool,
) -> HashMap<&'n str, &'n str> {
    if mark {
        for result in entry.tool_results() {
            if let Some(slot) = names.get_mut(result.tool_use_id.as_str()) {
                slot.referenced = true;
            }
        }
    }
    let names: &'n HashMap<String, ToolName> = names;
    entry
        .tool_results()
        .filter_map(|result| {
            names
                .get_key_value(result.tool_use_id.as_str())
                .map(|(id, slot)| (id.as_str(), slot.name.as_str()))
        })
        .collect()
}

fn parse(
    bytes: &[u8],
    budget: &mut ScanBudget<'_>,
    cancel: &Cancellation,
) -> Result<Option<Entry>, SnapshotError> {
    budget.charge_parse(cancel)?;
    let mut entries = Vec::with_capacity(1);
    crate::parse::parse_line(bytes, &mut entries, &|_| true)
        .map_err(|error| SnapshotError::new(Status::ParseError, format!("{error:?}")))?;
    Ok(entries.pop())
}

fn digest(bytes: &[u8]) -> u64 {
    let mut hasher = DefaultHasher::new();
    bytes.hash(&mut hasher);
    hasher.finish()
}

fn revision(stamp: &SourceStamp) -> String {
    format!("{}:{}:{}", stamp.size, stamp.mtime_ns, stamp.ctime_ns)
}

fn span(line: LineSpan, digest: u64) -> Span {
    Span {
        offset: line.offset,
        len: line.len,
        terminated: line.terminated,
        digest,
    }
}

fn charge_of(entry: &Entry) -> usize {
    let charge = entry_charge(entry);
    charge
        .owned_capacity_bytes
        .saturating_add(charge.opaque_dom_accounted_bytes)
}

fn saves(error: &SnapshotError) -> bool {
    matches!(
        error.status,
        Status::Incomplete
            | Status::OutputLimit
            | Status::RetainedLimit
            | Status::Deadline
            | Status::Cancelled
    )
}

impl<'store> GrepReducer<'store> {
    fn state(&self) -> ReducerState {
        ReducerState {
            counts: self.counts.clone(),
            matched_items: self.matched_items,
            coverage_complete: self.coverage_complete,
        }
    }

    fn keys(
        &self,
        store: &GrepCheckpoints,
        path: &Path,
        source: &SourceStream<'_>,
        render_names: bool,
    ) -> Result<(String, String), SnapshotError> {
        let canonical = std::fs::canonicalize(path)
            .map_err(|error| SnapshotError::new(Status::Incomplete, error.to_string()))?;
        let file = store
            .key(json!({
                "schema": crate::snapshot::SCHEMA,
                "classifier": {"id": "native", "version": "1"},
                "path": canonical.to_string_lossy().as_ref(),
                "device": source.stamp.identity.device.to_string(),
                "inode": source.stamp.identity.inode.to_string(),
            }))
            .map_err(|error| SnapshotError::new(Status::InvalidRequest, error))?;
        let query = store
            .key(json!({
                "file": file,
                "registry": self.registry.fingerprint(),
                "patterns": self.patterns.iter().map(|pattern| json!([pattern.id.to_string(), pattern.regex.as_str(), pattern.max_matches.map(|cap| cap.to_string())])).collect::<Vec<_>>(),
                "options": {
                    "kinds": self.options.kinds,
                    "tool": self.options.tool,
                    "ignore_case": self.options.ignore_case,
                    "where": [self.options.where_text, self.options.where_thinking, self.options.where_tools],
                    "context": self.options.context.to_string(),
                },
                "render_names": render_names,
                "start": sonic_rs::to_string(&self.state()).expect("reducer state is json"),
            }))
            .map_err(|error| SnapshotError::new(Status::InvalidRequest, error))?;
        Ok((file, query))
    }

    pub fn scan_stream<E>(
        &mut self,
        path: &Path,
        render_names: bool,
        checkpoints: Option<&GrepCheckpoints>,
        budget: &mut ScanBudget<'store>,
        cancel: &Cancellation,
        mut emit: E,
    ) -> Result<Option<ScanControl>, SnapshotError>
    where
        E: FnMut(
            GrepEvent<'_>,
            &mut ScanBudget<'store>,
            &Cancellation,
        ) -> Result<(), SnapshotError>,
    {
        let mut source = SourceStream::open(path, 0, budget, cancel)?;
        let generation = source.generation();
        let keys = checkpoints
            .map(|store| self.keys(store, path, &source, render_names))
            .transpose()?;
        let mut stream = GrepStream {
            query_key: keys.as_ref().map(|(_, query)| query.clone()),
            size: source.stamp.size,
            revision: revision(&source.stamp),
            source_bytes: path.as_os_str().len().saturating_add(generation.len()),
            generation,
            render_names,
            names: HashMap::new(),
            staging: budget.reserve_staging(0, cancel)?,
            queue: VecDeque::new(),
            history: Vec::new(),
            parsed: 0,
            decided: 0,
            emitted: 0,
            last_emitted: None,
            last_hit: None,
            stopped: None,
            committed: 0,
            fence: Vec::new(),
            sniffed: false,
            names_base: 0,
            names_final: false,
            capture: None,
            poisoned: false,
            prefix: Prefix::new(checkpoints.map_or(PREFIX_SEGMENT, GrepCheckpoints::segment)),
            proof: None,
            frozen: false,
            tail: None,
        };
        let mut existing = None;
        if let (Some(store), Some((file_key, query_key))) = (checkpoints, &keys) {
            let (record, bytes) = store.load(file_key, budget.remaining().max_read_bytes);
            budget.charge_projection(bytes, 0, cancel)?;
            if let Some(record) = record {
                let layer = record
                    .queries
                    .iter()
                    .find(|layer| {
                        &layer.key == query_key && layer.committed <= record.file.committed
                    })
                    .cloned();
                let validity = match stream.validates(&record, &mut source, budget, cancel)? {
                    Validity::Valid => match layer {
                        Some(layer) => {
                            let restored = self.restore(
                                &mut stream,
                                &mut source,
                                &record.file,
                                layer,
                                budget,
                                cancel,
                                &mut emit,
                            );
                            if stream.poisoned {
                                store.discard(file_key);
                            }
                            if restored? {
                                Validity::Valid
                            } else {
                                Validity::Invalid
                            }
                        }
                        None if record.file.committed == stream.size => {
                            stream.adopt(&record.file, budget, cancel)?;
                            Validity::Valid
                        }
                        None => Validity::Valid,
                    },
                    unproven => unproven,
                };
                match validity {
                    Validity::Valid => existing = Some(record),
                    Validity::Invalid => {
                        store.discard(file_key);
                        budget.progress.cache_invalidations += 1;
                    }
                    Validity::Unverified => {}
                }
            }
        }
        let result = self.drive(&mut stream, &mut source, budget, cancel, &mut emit);
        if let (Some(store), Some((file_key, _)), false) = (checkpoints, &keys, stream.poisoned) {
            if result.as_ref().map_or_else(saves, Option::is_some) {
                if let Some((file, query)) = stream.capture.take().or_else(|| stream.capture(self))
                {
                    let _ = store.save(&SourceRecord::merge(
                        existing,
                        file_key.clone(),
                        stream.size,
                        stream.revision.clone(),
                        file,
                        query,
                    ));
                }
            }
        }
        if stream.poisoned {
            if let (Some(store), Some((file_key, _))) = (checkpoints, &keys) {
                store.discard(file_key);
            }
        }
        let Some(eof) = result? else {
            return Ok(None);
        };
        if let Some(grown) = source.verify()? {
            stream.reverify(grown, &mut source, budget, cancel)?;
        }
        let names_used = stream.names.values().any(|slot| slot.referenced);
        Ok(Some(match stream.stopped {
            Some(hit) => ScanControl::Stop {
                source_complete: eof && stream.parsed == hit + 1 && self.coverage_complete,
                names_through: (names_used && !eof && !stream.names_final)
                    .then(|| stream.committed.max(stream.names_base)),
            },
            None => ScanControl::Continue,
        }))
    }

    fn drive<E>(
        &mut self,
        stream: &mut GrepStream<'store>,
        source: &mut SourceStream<'store>,
        budget: &mut ScanBudget<'store>,
        cancel: &Cancellation,
        emit: &mut E,
    ) -> Result<Option<bool>, SnapshotError>
    where
        E: FnMut(
            GrepEvent<'_>,
            &mut ScanBudget<'store>,
            &Cancellation,
        ) -> Result<(), SnapshotError>,
    {
        stream.decide(self, false, budget, cancel)?;
        stream.emit(self, false, budget, cancel, emit)?;
        while !stream.finished(self.options.context) {
            let Some(line) = source.next_line(budget, cancel)? else {
                if stream.capture.is_none() {
                    stream.capture = stream.capture(self);
                }
                stream.decide(self, true, budget, cancel)?;
                stream.emit(self, true, budget, cancel, emit)?;
                return Ok(Some(true));
            };
            if !line.terminated {
                stream.capture = stream.capture(self);
            }
            let bytes = source.bytes(&line);
            if !line.terminated {
                stream.tail = Some((line, digest(bytes)));
            }
            if !stream.sniffed && !bytes.iter().all(u8::is_ascii_whitespace) {
                stream.sniffed = true;
                if sniff_provider(bytes) == Provider::Codex {
                    return Ok(None);
                }
            }
            if let Some(entry) = parse(bytes, budget, cancel)? {
                stream.push(line, digest(bytes), entry, budget, cancel)?;
            }
            if line.terminated {
                stream.commit(line, source, budget, cancel)?;
            }
            stream.decide(self, false, budget, cancel)?;
            stream.emit(self, false, budget, cancel, emit)?;
        }
        Ok(Some(false))
    }

    fn restore<E>(
        &mut self,
        stream: &mut GrepStream<'store>,
        source: &mut SourceStream<'store>,
        file: &FileLayer,
        layer: QueryLayer,
        budget: &mut ScanBudget<'store>,
        cancel: &Cancellation,
        emit: &mut E,
    ) -> Result<bool, SnapshotError>
    where
        E: FnMut(
            GrepEvent<'_>,
            &mut ScanBudget<'store>,
            &Cancellation,
        ) -> Result<(), SnapshotError>,
    {
        let mut lines = Vec::with_capacity(layer.queue.len() + layer.replay.len());
        for span in layer
            .queue
            .iter()
            .map(|queued| queued.span)
            .chain(layer.replay.iter().map(|replayed| replayed.span))
        {
            if span.offset + span.len as u64 > layer.committed {
                return Ok(false);
            }
            let bytes = source.read_span(&span.into(), budget, cancel)?;
            if digest(&bytes) != span.digest {
                return Ok(false);
            }
            lines.push(bytes);
        }
        budget.progress.cache_hits += 1;
        budget.extend_staging(
            &mut stream.staging,
            layer.replay.len() * size_of::<Replayed>(),
            cancel,
        )?;
        stream.adopt(file, budget, cancel)?;
        for (id, used) in &layer.referenced {
            match stream.names.get_mut(id) {
                Some(slot) if slot.name == *used => slot.referenced = true,
                _ => {
                    stream.poisoned = true;
                    return Err(incomplete(
                        "tool_use id redefined with a different name after use",
                    ));
                }
            }
        }
        let mut lines = lines.into_iter();
        for (queued, bytes) in layer.queue.iter().zip(lines.by_ref()) {
            let entry = parse(&bytes, budget, cancel)?.ok_or_else(|| {
                SnapshotError::new(Status::Changed, "checkpointed event no longer parses")
            })?;
            let charge = charge_of(&entry);
            stream.queue.push_back(StreamSlot {
                index: queued.index,
                line: queued.span.into(),
                digest: queued.span.digest,
                entry,
                charge,
                pattern_ids: queued.pattern_ids.clone(),
                staging: budget
                    .reserve_staging(charge.saturating_add(size_of::<StreamSlot>()), cancel)?,
            });
        }
        self.counts = layer.reducer.counts;
        self.matched_items = layer.reducer.matched_items;
        self.coverage_complete = layer.reducer.coverage_complete;
        stream.parsed = layer.parsed;
        stream.decided = layer.decided;
        stream.emitted = layer.emitted;
        stream.last_emitted = layer.last_emitted;
        stream.last_hit = layer.last_hit;
        stream.stopped = layer.stopped;
        stream.committed = layer.committed;
        stream.sniffed = file.sniffed;
        source.seek(layer.committed)?;
        let results = HashMap::new();
        for (position, (replayed, bytes)) in layer.replay.iter().zip(lines).enumerate() {
            let entry = parse(&bytes, budget, cancel)?.ok_or_else(|| {
                SnapshotError::new(Status::Changed, "checkpointed event no longer parses")
            })?;
            let names = resolve(&mut stream.names, &entry, false);
            emit(
                GrepEvent {
                    index: replayed.index,
                    entry: &entry,
                    pattern_ids: replayed.pattern_ids.as_deref(),
                    names: &names,
                    results: &results,
                    generation: &stream.generation,
                    opens_source: position == 0,
                    opens_window: replayed.opens_window,
                    charge: charge_of(&entry),
                    source_bytes: stream.source_bytes,
                },
                budget,
                cancel,
            )?;
        }
        stream.history = layer.replay;
        Ok(true)
    }
}

impl<'store> GrepStream<'store> {
    fn finished(&self, context: usize) -> bool {
        self.stopped
            .is_some_and(|hit| self.parsed > hit + context.max(1) && self.emitted > hit + context)
    }

    fn adopt(
        &mut self,
        file: &FileLayer,
        budget: &mut ScanBudget<'store>,
        cancel: &Cancellation,
    ) -> Result<(), SnapshotError> {
        budget.extend_staging(
            &mut self.staging,
            file.names
                .iter()
                .map(|(id, name)| id.len() + name.len() + size_of::<(String, ToolName)>())
                .sum(),
            cancel,
        )?;
        self.names = file
            .names
            .iter()
            .map(|(id, name)| {
                (
                    id.clone(),
                    ToolName {
                        name: name.clone(),
                        referenced: false,
                    },
                )
            })
            .collect();
        self.names_base = file.committed;
        self.names_final = file.committed == self.size;
        self.prefix = self
            .proof
            .take()
            .unwrap_or_else(|| Prefix::adopt(self.prefix.span(), &file.prefix));
        Ok(())
    }

    fn validates(
        &mut self,
        record: &SourceRecord,
        source: &mut SourceStream<'store>,
        budget: &mut ScanBudget<'store>,
        cancel: &Cancellation,
    ) -> Result<Validity, SnapshotError> {
        let file = &record.file;
        if self.size < record.size
            || self.size == record.size && self.revision != record.revision
            || file.committed > record.size
            || file.fence.len() > FENCE_BYTES
            || file.fence.len() as u64 > file.committed
            || file.prefix.last().map_or(0, |(end, _)| *end) != file.committed
        {
            return Ok(Validity::Invalid);
        }
        let fence = source.read_span(
            &LineSpan {
                offset: file.committed - file.fence.len() as u64,
                len: file.fence.len(),
                terminated: false,
            },
            budget,
            cancel,
        )?;
        if fence != file.fence {
            return Ok(Validity::Invalid);
        }
        if self.size == record.size {
            return Ok(Validity::Valid);
        }
        if file.committed > budget.validation_remaining() as u64 {
            return Ok(Validity::Unverified);
        }
        self.proof = source.validate_prefix(&file.prefix, self.prefix.span(), budget, cancel)?;
        Ok(if self.proof.is_some() {
            Validity::Valid
        } else {
            Validity::Invalid
        })
    }

    fn reverify(
        &self,
        grown: [SourceStamp; 2],
        source: &mut SourceStream<'store>,
        budget: &mut ScanBudget<'store>,
        cancel: &Cancellation,
    ) -> Result<(), SnapshotError> {
        let tail = self.tail.map_or(0, |(line, _)| line.len);
        if self.frozen || self.prefix.end() as usize + tail > budget.validation_remaining() {
            return Err(incomplete(
                "source grew during the scan and its prefix exceeds the validation cap",
            ));
        }
        let intact = source
            .validate_prefix(&self.prefix.segments(), self.prefix.span(), budget, cancel)?
            .is_some()
            && match self.tail {
                Some((line, sum)) => digest(&source.revalidate_span(&line, budget, cancel)?) == sum,
                None => true,
            }
            && source.observe()? == grown;
        if intact {
            Ok(())
        } else {
            Err(SnapshotError::new(
                Status::Changed,
                "source rewritten while it grew during the scan",
            ))
        }
    }

    fn capture(&self, grep: &GrepReducer<'store>) -> Option<(FileLayer, QueryLayer)> {
        if self.frozen {
            return None;
        }
        Some((
            FileLayer {
                committed: self.committed,
                fence: self.fence.clone(),
                sniffed: self.sniffed,
                events: self.parsed,
                names: self
                    .names
                    .iter()
                    .map(|(id, slot)| (id.clone(), slot.name.clone()))
                    .collect(),
                prefix: self.prefix.segments(),
            },
            QueryLayer {
                key: self.query_key.clone()?,
                committed: self.committed,
                parsed: self.parsed,
                decided: self.decided,
                emitted: self.emitted,
                last_emitted: self.last_emitted,
                last_hit: self.last_hit,
                stopped: self.stopped,
                reducer: grep.state(),
                referenced: self
                    .names
                    .iter()
                    .filter(|(_, slot)| slot.referenced)
                    .map(|(id, slot)| (id.clone(), slot.name.clone()))
                    .collect(),
                replay: self.history.clone(),
                queue: self
                    .queue
                    .iter()
                    .map(|slot| Queued {
                        index: slot.index,
                        span: span(slot.line, slot.digest),
                        pattern_ids: slot.pattern_ids.clone(),
                    })
                    .collect(),
            },
        ))
    }

    fn commit(
        &mut self,
        line: LineSpan,
        source: &mut SourceStream<'store>,
        budget: &mut ScanBudget<'store>,
        cancel: &Cancellation,
    ) -> Result<(), SnapshotError> {
        let extends = line.offset >= self.prefix.end() && !self.frozen;
        if let Some(open) = self.prefix.unread().filter(|_| extends) {
            if open.len > budget.validation_remaining() {
                self.frozen = true;
            } else if !self
                .prefix
                .seed(&source.revalidate_span(&open, budget, cancel)?)
            {
                self.poisoned = true;
                return Err(SnapshotError::new(
                    Status::Changed,
                    "source changed under its checkpoint",
                ));
            }
        }
        let bytes = source.bytes(&line);
        self.committed = line.end();
        if extends && !self.frozen {
            self.prefix.write(bytes);
            self.prefix.write(b"\n");
        }
        let tail = &bytes[bytes.len().saturating_sub(FENCE_BYTES - 1)..];
        self.fence.extend_from_slice(tail);
        self.fence.push(b'\n');
        let excess = self.fence.len().saturating_sub(FENCE_BYTES);
        self.fence.drain(..excess);
        Ok(())
    }

    fn push(
        &mut self,
        line: LineSpan,
        digest: u64,
        entry: Entry,
        budget: &mut ScanBudget<'store>,
        cancel: &Cancellation,
    ) -> Result<(), SnapshotError> {
        for block in entry.blocks() {
            let bytes = match block {
                ContentBlock::ToolUse(tool) => tool.id.len().saturating_add(tool.name.len()),
                ContentBlock::ToolResult(result) => result
                    .tool_use_id
                    .len()
                    .saturating_add(result.denial_kind.as_ref().map_or(0, String::len)),
                _ => 0,
            };
            budget.charge_projection(bytes.saturating_add(1), 0, cancel)?;
        }
        let mut names_bytes = 0usize;
        for tool in entry.tool_uses().filter(|_| !self.names_final) {
            match self.names.get(tool.id.as_str()) {
                Some(slot) if slot.name == tool.name => {}
                Some(slot) if slot.referenced => {
                    self.poisoned = true;
                    return Err(incomplete(
                        "tool_use id redefined with a different name after use",
                    ));
                }
                Some(_) => names_bytes = names_bytes.saturating_add(tool.name.len()),
                None => {
                    names_bytes = names_bytes
                        .saturating_add(tool.id.len())
                        .saturating_add(tool.name.len())
                        .saturating_add(size_of::<(String, ToolName)>())
                }
            }
        }
        let charge = charge_of(&entry);
        let staging =
            budget.reserve_staging(charge.saturating_add(size_of::<StreamSlot>()), cancel)?;
        budget.extend_staging(&mut self.staging, names_bytes, cancel)?;
        for tool in entry.tool_uses().filter(|_| !self.names_final) {
            self.names
                .entry(tool.id.clone())
                .and_modify(|slot| slot.name.clone_from(&tool.name))
                .or_insert_with(|| ToolName {
                    name: tool.name.clone(),
                    referenced: false,
                });
        }
        self.queue.push_back(StreamSlot {
            index: self.parsed,
            line,
            digest,
            entry,
            charge,
            pattern_ids: None,
            staging,
        });
        self.parsed += 1;
        Ok(())
    }

    fn decide(
        &mut self,
        grep: &mut GrepReducer<'store>,
        eof: bool,
        budget: &mut ScanBudget<'store>,
        cancel: &Cancellation,
    ) -> Result<(), SnapshotError> {
        let filtering = !grep.options.errors && grep.tool_names.is_some();
        let results = HashMap::new();
        while self.stopped.is_none() && self.decided < self.parsed {
            let position = self.decided - self.emitted;
            let slot = &self.queue[position];
            let lookup = filtering && matches!(slot.entry, Entry::User(_));
            if lookup && !eof && unresolved(&self.names, &slot.entry) {
                break;
            }
            let names = if lookup {
                resolve(&mut self.names, &slot.entry, true)
            } else {
                HashMap::new()
            };
            let matched =
                grep.matches(&slot.entry, slot.charge, &names, &results, budget, cancel)?;
            drop(names);
            if let Some((matched, _match_staging)) = matched {
                let slot = &mut self.queue[position];
                slot.pattern_ids =
                    Some(grep.commit_matches(&matched, &mut slot.staging, budget, cancel)?);
                if grep.satisfied() {
                    self.stopped = Some(slot.index);
                }
            }
            self.decided += 1;
        }
        Ok(())
    }

    fn emit<E>(
        &mut self,
        grep: &GrepReducer<'store>,
        eof: bool,
        budget: &mut ScanBudget<'store>,
        cancel: &Cancellation,
        emit: &mut E,
    ) -> Result<(), SnapshotError>
    where
        E: FnMut(
            GrepEvent<'_>,
            &mut ScanBudget<'store>,
            &Cancellation,
        ) -> Result<(), SnapshotError>,
    {
        let context = grep.options.context;
        let results = HashMap::new();
        while let Some(slot) = self.queue.front() {
            let index = slot.index;
            let finalized = match self.stopped {
                Some(hit) => index <= hit + context,
                None => index < self.decided && (eof || self.decided > index + context),
            };
            if !finalized {
                break;
            }
            let windowed = self.last_hit.is_some_and(|hit| index - hit <= context)
                || self
                    .queue
                    .iter()
                    .take(context + 1)
                    .any(|slot| slot.pattern_ids.is_some());
            if windowed {
                if self.render_names && !eof && unresolved(&self.names, &slot.entry) {
                    break;
                }
                let opens_window = self.last_emitted.is_none_or(|last| last + 1 != index);
                budget.extend_staging(&mut self.staging, size_of::<Replayed>(), cancel)?;
                let names = resolve(&mut self.names, &slot.entry, self.render_names);
                emit(
                    GrepEvent {
                        index,
                        entry: &slot.entry,
                        pattern_ids: slot.pattern_ids.as_deref(),
                        names: &names,
                        results: &results,
                        generation: &self.generation,
                        opens_source: self.last_emitted.is_none(),
                        opens_window,
                        charge: slot.charge,
                        source_bytes: self.source_bytes,
                    },
                    budget,
                    cancel,
                )?;
                self.history.push(Replayed {
                    index,
                    span: span(slot.line, slot.digest),
                    pattern_ids: slot.pattern_ids.clone(),
                    opens_window,
                });
                self.last_emitted = Some(index);
            }
            if slot.pattern_ids.is_some() {
                self.last_hit = Some(index);
            }
            self.queue.pop_front();
            self.emitted += 1;
        }
        Ok(())
    }
}
