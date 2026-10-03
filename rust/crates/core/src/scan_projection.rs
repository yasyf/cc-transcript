use crate::filter::{entry_text, event_kind};
use crate::render::{haystack, search_add, tool_haystack};
use crate::snapshot::SnapshotError;
use crate::types::{ContentBlock, Entry};

const KINDS: [&str; 6] = ["user", "assistant", "system", "mode", "other", "attachment"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    Text,
    Thinking,
    Tool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Projected {
    kind: u8,
    uses: Vec<String>,
    results: Vec<String>,
    parts: Vec<(Class, String)>,
}

struct Cursor<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Cursor<'a> {
    fn take(&mut self, len: usize) -> Option<&'a [u8]> {
        let bytes = self.bytes.get(self.at..self.at.checked_add(len)?)?;
        self.at += len;
        Some(bytes)
    }

    fn byte(&mut self) -> Option<u8> {
        self.take(1).map(|bytes| bytes[0])
    }

    fn word(&mut self) -> Option<usize> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?) as usize)
    }

    fn text(&mut self) -> Option<String> {
        let len = self.word()?;
        String::from_utf8(self.take(len)?.to_vec()).ok()
    }

    fn texts(&mut self) -> Option<Vec<String>> {
        (0..self.word()?).map(|_| self.text()).collect()
    }
}

fn put_text(out: &mut Vec<u8>, text: &str) {
    out.extend_from_slice(&(text.len() as u32).to_le_bytes());
    out.extend_from_slice(text.as_bytes());
}

fn put_texts(out: &mut Vec<u8>, texts: &[String]) {
    out.extend_from_slice(&(texts.len() as u32).to_le_bytes());
    for text in texts {
        put_text(out, text);
    }
}

pub fn grams(bytes: &[u8]) -> impl Iterator<Item = u32> + '_ {
    bytes
        .windows(4)
        .filter(|window| !window.contains(&b'\n'))
        .map(|window| u32::from_be_bytes(window.try_into().expect("four bytes")))
}

impl Projected {
    pub fn of(entry: &Entry) -> Self {
        let parts = match entry {
            Entry::User(_) | Entry::Assistant(_) => {
                std::iter::once((Class::Text, entry_text(entry)))
                    .chain(entry.blocks().iter().filter_map(|block| match block {
                        ContentBlock::Thinking(text) => Some((Class::Thinking, text.clone())),
                        _ => None,
                    }))
                    .chain(entry.blocks().iter().filter_map(|block| {
                        matches!(
                            block,
                            ContentBlock::ToolUse(_) | ContentBlock::ToolResult(_)
                        )
                        .then(|| (Class::Tool, tool_haystack(block)))
                    }))
                    .collect()
            }
            _ => vec![(Class::Text, haystack(entry, true, false, false))],
        };
        Self {
            kind: KINDS
                .iter()
                .position(|kind| *kind == event_kind(entry))
                .expect("event kinds are closed") as u8,
            uses: entry.tool_uses().map(|tool| tool.name.clone()).collect(),
            results: entry
                .tool_results()
                .map(|result| result.tool_use_id.clone())
                .collect(),
            parts,
        }
    }

    pub fn kind(&self) -> &'static str {
        KINDS[self.kind as usize]
    }

    pub fn uses(&self) -> impl Iterator<Item = &str> {
        self.uses.iter().map(String::as_str)
    }

    pub fn results(&self) -> impl Iterator<Item = &str> {
        self.results.iter().map(String::as_str)
    }

    pub fn bytes(&self) -> usize {
        self.parts.iter().map(|(_, part)| part.len()).sum::<usize>()
            + self
                .uses
                .iter()
                .chain(&self.results)
                .map(String::len)
                .sum::<usize>()
    }

    fn selected(&self, text: bool, thinking: bool, tools: bool) -> impl Iterator<Item = &str> {
        self.parts
            .iter()
            .filter(move |(class, part)| {
                !part.is_empty()
                    && match class {
                        Class::Text => text,
                        Class::Thinking => thinking,
                        Class::Tool => tools,
                    }
            })
            .map(|(_, part)| part.as_str())
    }

    pub fn haystack(&self, text: bool, thinking: bool, tools: bool) -> String {
        self.selected(text, thinking, tools)
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub fn haystack_bound(
        &self,
        text: bool,
        thinking: bool,
        tools: bool,
        limit: usize,
    ) -> Result<usize, SnapshotError> {
        let mut total = 0;
        for (position, part) in self.selected(text, thinking, tools).enumerate() {
            if position != 0 {
                search_add(&mut total, 1, limit)?;
            }
            search_add(&mut total, part.len(), limit)?;
        }
        Ok(total)
    }

    pub fn grams(&self) -> impl Iterator<Item = u32> + '_ {
        self.parts
            .iter()
            .flat_map(|(_, part)| grams(part.as_bytes()))
    }

    pub fn encode(&self, out: &mut Vec<u8>) {
        let start = out.len();
        out.extend_from_slice(&[0; 4]);
        out.push(self.kind);
        put_texts(out, &self.uses);
        put_texts(out, &self.results);
        out.extend_from_slice(&(self.parts.len() as u32).to_le_bytes());
        for (class, part) in &self.parts {
            out.push(*class as u8);
            put_text(out, part);
        }
        let len = (out.len() - start - 4) as u32;
        out[start..start + 4].copy_from_slice(&len.to_le_bytes());
    }

    pub fn decode_all(bytes: &[u8]) -> Option<Vec<Self>> {
        let mut cursor = Cursor { bytes, at: 0 };
        let mut events = Vec::new();
        while cursor.at < bytes.len() {
            let len = cursor.word()?;
            let mut record = Cursor {
                bytes: cursor.take(len)?,
                at: 0,
            };
            let kind = record
                .byte()
                .filter(|kind| (*kind as usize) < KINDS.len())?;
            let uses = record.texts()?;
            let results = record.texts()?;
            let parts = (0..record.word()?)
                .map(|_| {
                    let class = match record.byte()? {
                        0 => Class::Text,
                        1 => Class::Thinking,
                        2 => Class::Tool,
                        _ => return None,
                    };
                    Some((class, record.text()?))
                })
                .collect::<Option<Vec<_>>>()?;
            if record.at != len {
                return None;
            }
            events.push(Self {
                kind,
                uses,
                results,
                parts,
            });
        }
        Some(events)
    }
}
