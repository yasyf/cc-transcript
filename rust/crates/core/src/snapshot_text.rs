use std::collections::HashSet;
use std::mem::size_of;

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

#[derive(Serialize)]
pub struct SourceFacts {
    pub cwds: Vec<String>,
    pub first_user_contains: bool,
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
    let mut facts = SourceFacts {
        cwds: Vec::new(),
        first_user_contains: false,
    };
    output(
        usage,
        limits,
        snapshot_codec::encoded_size(&facts, limits.max_output_bytes)?,
        1,
    )?;
    let prefixes = prefixes(token.as_bytes());
    let mut seen = HashSet::new();
    let mut first_user_seen = false;
    for entry in snapshot
        .chunks
        .iter()
        .flat_map(|chunk| chunk.entries.iter())
    {
        cancel.check(limits.deadline_unix_ms)?;
        read(usage, limits, size_of::<usize>(), 1)?;
        if let Some(cwd) = entry.meta().and_then(|meta| meta.cwd.as_deref()) {
            read(usage, limits, cwd.len(), 0)?;
            if !seen.contains(cwd) {
                let bytes = snapshot_codec::encoded_size(
                    cwd,
                    limits.max_output_bytes.saturating_sub(usage.output_bytes),
                )?
                .saturating_add(usize::from(!facts.cwds.is_empty()));
                output(usage, limits, bytes, 0)?;
                seen.insert(cwd);
                facts.cwds.push(cwd.to_owned());
            }
        }
        if !first_user_seen {
            if let Entry::User(user) = entry {
                first_user_seen = true;
                if token.is_empty() {
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
                facts.first_user_contains = parts.contains(token.as_bytes(), &prefixes);
            }
        }
    }
    cancel.check(limits.deadline_unix_ms)?;
    let encoded = snapshot_codec::encode(&facts, limits.max_output_bytes)?;
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
        .saturating_add(token_bytes.saturating_mul(size_of::<usize>()))
        .saturating_add(8192)
}

#[cfg(test)]
mod tests {
    use super::*;

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
