use std::collections::HashSet;
use std::mem::size_of;
use std::sync::atomic::{AtomicUsize, Ordering};

use serde::Serialize;

use crate::snapshot::{Cancellation, SnapshotError, Status, TranscriptSnapshot, WorkLimits};
use crate::snapshot_codec;
use crate::types::{ContentBlock, Entry, EntryMeta, UserContent};

const PAGE_EVENTS: usize = 256;

#[derive(Default)]
pub struct TextUsage {
    pub read_bytes: usize,
    pub events: usize,
    pub items: usize,
    pub output_bytes: usize,
}

#[derive(Default, serde::Serialize, serde::Deserialize)]
pub struct SourceFacts {
    pub cwds: Vec<String>,
    pub first_user_contains: bool,
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceFactsIndex {
    token: String,
    prefixes: Vec<usize>,
    seen: HashSet<String>,
    first_user_seen: bool,
    facts: SourceFacts,
    #[serde(skip)]
    accounted: AtomicUsize,
    #[serde(skip)]
    false_encoded: AtomicUsize,
    #[cfg(test)]
    #[serde(skip)]
    size_audits: AtomicUsize,
}

impl SourceFactsIndex {
    pub(crate) fn append_bound(&self, entry: &Entry) -> usize {
        use crate::snapshot_ledger::{set_growth, vec_growth};
        entry
            .meta()
            .and_then(|meta| meta.cwd.as_ref())
            .filter(|cwd| !self.seen.contains(*cwd))
            .map_or(0, |cwd| {
                set_growth(&self.seen, 1) + vec_growth(&self.facts.cwds, 1) + cwd.len() * 4 + 256
            })
    }

    pub fn new(token: &str) -> Self {
        Self {
            token: token.to_owned(),
            prefixes: prefixes(token.as_bytes()),
            seen: HashSet::new(),
            first_user_seen: false,
            facts: SourceFacts::default(),
            accounted: AtomicUsize::new(0),
            false_encoded: AtomicUsize::new(0),
            #[cfg(test)]
            size_audits: AtomicUsize::new(0),
        }
    }

    pub fn facts(&self) -> &SourceFacts {
        &self.facts
    }

    pub fn accounted_bytes(&self) -> usize {
        let cached = self.accounted.load(Ordering::Relaxed);
        if cached != 0 {
            return cached;
        }
        let bytes = size_of::<Self>()
            + self.token.capacity()
            + self.prefixes.capacity() * size_of::<usize>()
            + self.seen.capacity() * size_of::<String>()
            + self.seen.iter().map(String::capacity).sum::<usize>()
            + self.facts.cwds.capacity() * size_of::<String>()
            + self.facts.cwds.iter().map(String::capacity).sum::<usize>();
        self.accounted.store(bytes, Ordering::Relaxed);
        bytes
    }

    fn false_encoded_bytes(&self) -> Result<usize, SnapshotError> {
        let cached = self.false_encoded.load(Ordering::Relaxed);
        if cached != 0 {
            return Ok(cached);
        }
        let bytes = snapshot_codec::encoded_size(&self.facts, usize::MAX)?
            + usize::from(self.facts.first_user_contains);
        #[cfg(test)]
        self.size_audits.fetch_add(1, Ordering::Relaxed);
        self.false_encoded.store(bytes, Ordering::Relaxed);
        Ok(bytes)
    }

    pub fn append<'a>(
        &mut self,
        entries: impl IntoIterator<Item = &'a Entry>,
        limits: &WorkLimits,
        cancel: &Cancellation,
        usage: &mut TextUsage,
    ) -> Result<(), SnapshotError> {
        cancel.check(limits.deadline_unix_ms)?;
        let mut false_encoded = self.false_encoded_bytes()?;
        output(usage, limits, false_encoded, 1)?;
        let mut cwds = Vec::new();
        let mut seen = HashSet::new();
        let mut first_user_seen = self.first_user_seen;
        let mut first_user_contains = self.facts.first_user_contains;
        for entry in entries {
            cancel.check(limits.deadline_unix_ms)?;
            read(usage, limits, size_of::<usize>(), 1)?;
            if let Some(cwd) = entry.meta().and_then(|meta| meta.cwd.as_deref()) {
                read(usage, limits, cwd.len(), 0)?;
                if !self.seen.contains(cwd) && seen.insert(cwd) {
                    let bytes = snapshot_codec::encoded_size(
                        cwd,
                        limits.max_output_bytes.saturating_sub(usage.output_bytes),
                    )?
                    .saturating_add(usize::from(!self.facts.cwds.is_empty() || !cwds.is_empty()));
                    output(usage, limits, bytes, 0)?;
                    false_encoded += bytes;
                    cwds.push(cwd);
                }
            }
            if !first_user_seen {
                if let Entry::User(user) = entry {
                    first_user_seen = true;
                    if self.token.is_empty() {
                        continue;
                    }
                    let parts = TextParts::user(&user.content);
                    read(
                        usage,
                        limits,
                        parts.blocks().saturating_mul(size_of::<usize>()),
                        0,
                    )?;
                    read(usage, limits, parts.bytes(), 0)?;
                    first_user_contains = parts.contains(self.token.as_bytes(), &self.prefixes);
                }
            }
        }
        cancel.check(limits.deadline_unix_ms)?;
        let mut accounted = self.accounted_bytes();
        let old_seen_capacity = self.seen.capacity();
        let old_cwds_capacity = self.facts.cwds.capacity();
        for cwd in cwds {
            let key = cwd.to_owned();
            let value = cwd.to_owned();
            accounted += key.capacity() + value.capacity();
            self.seen.insert(key);
            self.facts.cwds.push(value);
        }
        accounted += (self.seen.capacity() - old_seen_capacity) * size_of::<String>()
            + (self.facts.cwds.capacity() - old_cwds_capacity) * size_of::<String>();
        self.first_user_seen = first_user_seen;
        self.facts.first_user_contains = first_user_contains;
        self.accounted.store(accounted, Ordering::Relaxed);
        self.false_encoded.store(false_encoded, Ordering::Relaxed);
        Ok(())
    }
}

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ProseRole {
    User,
    Assistant,
}

#[derive(Serialize)]
pub struct ProseRow {
    pub event_index: usize,
    pub role: ProseRole,
    pub text: String,
    pub is_sidechain: bool,
    pub is_meta: bool,
}

#[derive(Serialize)]
struct ProsePage {
    rows: Vec<ProseRow>,
    next: Option<usize>,
}

#[derive(Clone)]
enum TextParts<'a> {
    Plain(Option<&'a str>),
    Blocks(std::slice::Iter<'a, ContentBlock>),
}

impl<'a> Iterator for TextParts<'a> {
    type Item = &'a str;

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            Self::Plain(text) => text.take(),
            Self::Blocks(blocks) => blocks.find_map(|block| match block {
                ContentBlock::Text(text) => Some(text.as_str()),
                _ => None,
            }),
        }
    }
}

impl<'a> TextParts<'a> {
    fn user(content: &'a UserContent) -> Self {
        match content {
            UserContent::Plain(text) => Self::Plain(Some(text)),
            UserContent::Blocks(blocks) => Self::Blocks(blocks.iter()),
        }
    }

    fn blocks(&self) -> usize {
        match self {
            Self::Plain(_) => 1,
            Self::Blocks(blocks) => blocks.len(),
        }
    }

    fn bytes(&self) -> usize {
        self.clone()
            .enumerate()
            .fold(0usize, |bytes, (index, text)| {
                bytes
                    .saturating_add(text.len())
                    .saturating_add(usize::from(index != 0))
            })
    }

    fn encoded_extra(&self, limit: usize) -> Result<usize, SnapshotError> {
        let mut bytes = 0usize;
        for (index, text) in self.clone().enumerate() {
            let separator = usize::from(index != 0);
            if separator > limit.saturating_sub(bytes) {
                return Err(SnapshotError::new(
                    Status::OutputLimit,
                    "prose separator exceeds output budget",
                ));
            }
            bytes += separator;
            let encoded =
                snapshot_codec::encoded_size(text, limit.saturating_sub(bytes).saturating_add(2))?;
            bytes += encoded - 2;
        }
        Ok(bytes)
    }

    fn joined(self, bytes: usize) -> String {
        let mut text = String::with_capacity(bytes);
        for (index, part) in self.enumerate() {
            if index != 0 {
                text.push(' ');
            }
            text.push_str(part);
        }
        text
    }

    fn contains(self, token: &[u8], prefixes: &[usize]) -> bool {
        if token.is_empty() {
            return false;
        }
        let mut matched = 0;
        for byte in self
            .enumerate()
            .flat_map(|(index, text)| (index != 0).then_some(b' ').into_iter().chain(text.bytes()))
        {
            while matched > 0 && token[matched] != byte {
                matched = prefixes[matched - 1];
            }
            if token[matched] == byte {
                matched += 1;
            }
            if matched == token.len() {
                return true;
            }
        }
        false
    }
}

fn prefixes(token: &[u8]) -> Vec<usize> {
    let mut prefixes = vec![0; token.len()];
    let mut matched = 0;
    for index in 1..token.len() {
        while matched > 0 && token[matched] != token[index] {
            matched = prefixes[matched - 1];
        }
        if token[matched] == token[index] {
            matched += 1;
        }
        prefixes[index] = matched;
    }
    prefixes
}

fn read(
    usage: &mut TextUsage,
    limits: &WorkLimits,
    bytes: usize,
    events: usize,
) -> Result<(), SnapshotError> {
    if bytes > limits.max_read_bytes.saturating_sub(usage.read_bytes) {
        return Err(SnapshotError::new(
            Status::SourceLimit,
            "narrow text read budget exceeded",
        ));
    }
    if events > limits.max_events.saturating_sub(usage.events) {
        return Err(SnapshotError::new(
            Status::EntryLimit,
            "narrow text event budget exceeded",
        ));
    }
    usage.read_bytes += bytes;
    usage.events += events;
    Ok(())
}

fn output(
    usage: &mut TextUsage,
    limits: &WorkLimits,
    bytes: usize,
    items: usize,
) -> Result<(), SnapshotError> {
    if bytes > limits.max_output_bytes.saturating_sub(usage.output_bytes)
        || items > limits.max_items.saturating_sub(usage.items)
    {
        return Err(SnapshotError::new(
            Status::OutputLimit,
            "narrow text output budget exceeded",
        ));
    }
    usage.output_bytes += bytes;
    usage.items += items;
    Ok(())
}

pub fn source_facts(
    snapshot: &TranscriptSnapshot,
    token: &str,
    limits: &WorkLimits,
    cancel: &Cancellation,
    usage: &mut TextUsage,
) -> Result<String, SnapshotError> {
    let capped = WorkLimits {
        max_output_bytes: limits
            .max_output_bytes
            .min(snapshot_codec::MAX_RECORD_BYTES),
        ..*limits
    };
    let limits = &capped;
    cancel.check(limits.deadline_unix_ms)?;
    read(usage, limits, token.len(), 0)?;
    let mut index = SourceFactsIndex::new(token);
    index.append(
        snapshot
            .chunks
            .iter()
            .flat_map(|chunk| chunk.entries.iter()),
        limits,
        cancel,
        usage,
    )?;
    let encoded = snapshot_codec::encode(index.facts(), limits.max_output_bytes)?;
    usage.output_bytes = encoded.len();
    Ok(encoded)
}

fn prose_parts(entry: &Entry) -> Option<(ProseRole, &EntryMeta, TextParts<'_>)> {
    match entry {
        Entry::User(user) => Some((ProseRole::User, &user.meta, TextParts::user(&user.content))),
        Entry::Assistant(assistant) => Some((
            ProseRole::Assistant,
            &assistant.meta,
            TextParts::Blocks(assistant.blocks.iter()),
        )),
        _ => None,
    }
}

pub fn prose_page(
    snapshot: &TranscriptSnapshot,
    start: usize,
    limits: &WorkLimits,
    cancel: &Cancellation,
    usage: &mut TextUsage,
) -> Result<String, SnapshotError> {
    let capped = WorkLimits {
        max_output_bytes: limits
            .max_output_bytes
            .min(snapshot_codec::MAX_RECORD_BYTES),
        ..*limits
    };
    let limits = &capped;
    cancel.check(limits.deadline_unix_ms)?;
    if start > snapshot.event_count {
        return Err(SnapshotError::new(
            Status::InvalidRequest,
            "prose position outside snapshot",
        ));
    }
    let stop = snapshot.event_count.min(start.saturating_add(PAGE_EVENTS));
    let mut page = ProsePage {
        rows: Vec::new(),
        next: (stop < snapshot.event_count).then_some(stop),
    };
    output(
        usage,
        limits,
        snapshot_codec::encoded_size(&page, limits.max_output_bytes)?.saturating_add(20),
        0,
    )?;
    for index in start..stop {
        cancel.check(limits.deadline_unix_ms)?;
        read(usage, limits, size_of::<usize>(), 1)?;
        let Some((role, meta, parts)) = prose_parts(snapshot.entry(index)) else {
            continue;
        };
        read(
            usage,
            limits,
            parts
                .blocks()
                .saturating_mul(size_of::<usize>())
                .saturating_add(2),
            0,
        )?;
        let bytes = parts.bytes();
        read(usage, limits, bytes, 0)?;
        if !parts
            .clone()
            .any(|part| !crate::pystr::strip(part).is_empty())
        {
            continue;
        }
        if usage.items == limits.max_items {
            if page.rows.is_empty() {
                return Err(SnapshotError::new(
                    Status::OutputLimit,
                    "narrow text item budget exhausted",
                ));
            }
            page.next = Some(index);
            break;
        }
        let mut row = ProseRow {
            event_index: index,
            role,
            text: String::new(),
            is_sidechain: meta.is_sidechain,
            is_meta: meta.is_meta,
        };
        let available = limits.max_output_bytes.saturating_sub(usage.output_bytes);
        let size: Result<usize, SnapshotError> = (|| {
            let base = snapshot_codec::encoded_size(&row, available)?
                .saturating_add(usize::from(!page.rows.is_empty()));
            Ok(base.saturating_add(parts.encoded_extra(available.saturating_sub(base))?))
        })();
        let size = match size {
            Ok(size) => size,
            Err(failure) if failure.status == Status::OutputLimit && !page.rows.is_empty() => {
                page.next = Some(index);
                break;
            }
            Err(failure) => return Err(failure),
        };
        output(usage, limits, size, 1)?;
        row.text = parts.joined(bytes);
        page.rows.push(row);
    }
    cancel.check(limits.deadline_unix_ms)?;
    let encoded = snapshot_codec::encode(&page, limits.max_output_bytes)?;
    usage.output_bytes = encoded.len();
    Ok(encoded)
}

pub fn staging_bytes(limits: &WorkLimits, token_bytes: usize) -> usize {
    limits
        .max_output_bytes
        .min(snapshot_codec::MAX_RECORD_BYTES)
        .saturating_mul(4)
        .saturating_add(
            limits
                .max_events
                .min(limits.max_output_bytes)
                .saturating_mul(64),
        )
        .saturating_add(token_bytes.saturating_mul(size_of::<usize>() + 1))
        .saturating_add(8192)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits() -> WorkLimits {
        WorkLimits {
            max_read_bytes: 65536,
            max_source_read_bytes: 65536,
            max_events: 100,
            max_items: 100,
            max_output_bytes: 65536,
            max_discovery_entries: 100,
            max_sources: 2,
            deadline_unix_ms: u64::MAX,
        }
    }

    fn user(uuid: &str, text: &str, cwd: &str) -> Entry {
        let raw = sonic_rs::json!({
            "type":"user", "sessionId":"review-source", "uuid":uuid,
            "timestamp":"2026-11-02T00:00:00Z", "cwd":cwd,
            "message":{"role":"user","content":text}
        });
        crate::parse::parse_bytes(&sonic_rs::to_vec(&raw).unwrap(), |_| true)
            .unwrap()
            .pop()
            .unwrap()
    }

    #[test]
    fn source_facts_survive_persistence_and_append_without_revisiting_old_users() {
        let first = user("first", "ordinary first user", "/first");
        let second = user("second", "reviewer-marker", "/second");
        let repeated = user("third", "reviewer-marker", "/first");
        let cancel = Cancellation::default();
        let mut index = SourceFactsIndex::new("reviewer-marker");
        index
            .append([&first], &limits(), &cancel, &mut TextUsage::default())
            .unwrap();
        let encoded = sonic_rs::to_vec(&index).unwrap();
        let mut index: SourceFactsIndex = sonic_rs::from_slice(&encoded).unwrap();
        let mut usage = TextUsage::default();
        index
            .append([&second, &repeated], &limits(), &cancel, &mut usage)
            .unwrap();
        assert_eq!(usage.events, 2);
        assert_eq!(index.facts().cwds, ["/first", "/second"]);
        assert!(!index.facts().first_user_contains);
        let size_audits = index.size_audits.load(Ordering::Relaxed);
        let mut unchanged = TextUsage::default();
        index
            .append([], &limits(), &cancel, &mut unchanged)
            .unwrap();
        assert_eq!((unchanged.events, unchanged.read_bytes), (0, 0));
        assert_eq!(index.size_audits.load(Ordering::Relaxed), size_audits);
    }

    #[test]
    fn source_facts_true_match_preserves_false_preflight_budget() {
        let first = user("first", "reviewer-marker", "/first");
        let mut index = SourceFactsIndex::new("reviewer-marker");
        index
            .append(
                [&first],
                &limits(),
                &Cancellation::default(),
                &mut TextUsage::default(),
            )
            .unwrap();
        let actual = snapshot_codec::encoded_size(index.facts(), usize::MAX).unwrap();
        let tight = WorkLimits {
            max_output_bytes: actual,
            ..limits()
        };
        let error = index
            .append(
                [],
                &tight,
                &Cancellation::default(),
                &mut TextUsage::default(),
            )
            .unwrap_err();
        assert_eq!(error.status, Status::OutputLimit);
        let exact = WorkLimits {
            max_output_bytes: actual + 1,
            ..limits()
        };
        index
            .append(
                [],
                &exact,
                &Cancellation::default(),
                &mut TextUsage::default(),
            )
            .unwrap();
    }

    #[test]
    fn source_facts_staging_accounts_for_owned_token_and_prefixes() {
        let token_bytes = 65536;
        let tight = WorkLimits {
            max_output_bytes: 64,
            max_events: 1,
            ..limits()
        };
        assert!(staging_bytes(&tight, token_bytes) >= token_bytes * (size_of::<usize>() + 1));
    }

    #[test]
    fn source_facts_budget_failure_preserves_committed_state() {
        let first = user("first", "reviewer-marker", "/first");
        let mut index = SourceFactsIndex::new("reviewer-marker");
        let before = sonic_rs::to_vec(&index).unwrap();
        let exhausted = WorkLimits {
            max_read_bytes: 0,
            ..limits()
        };
        assert!(index
            .append(
                [&first],
                &exhausted,
                &Cancellation::default(),
                &mut TextUsage::default()
            )
            .is_err());
        assert_eq!(sonic_rs::to_vec(&index).unwrap(), before);
        index
            .append(
                [&first],
                &limits(),
                &Cancellation::default(),
                &mut TextUsage::default(),
            )
            .unwrap();
        assert!(index.facts().first_user_contains);
    }

    #[test]
    fn streaming_contains_matches_exact_joined_text_across_empty_and_unicode_parts() {
        let parts = ["", "ab", "", "é日", "end", ""];
        let blocks: Vec<_> = parts
            .iter()
            .map(|part| ContentBlock::Text((*part).to_owned()))
            .collect();
        let joined = crate::types::joined_text(&blocks);
        for token in ["", "ab  é", "é日 end", "日 en", "ab é", "missing", " "] {
            assert_eq!(
                TextParts::Blocks(blocks.iter())
                    .contains(token.as_bytes(), &prefixes(token.as_bytes())),
                !token.is_empty() && joined.contains(token)
            );
        }
        assert_eq!(
            TextParts::Blocks(blocks.iter()).joined(joined.len()),
            joined
        );
    }
}
