use memchr::memchr_iter;
use sonic_rs::Value;

use crate::activity::{session_activity, ActivityOpts, SessionActivity};
use crate::codex::{codex_session_activity, lower, parse_codex_bytes, CodexSession};
use crate::parse::{parse_bytes, ParseError};
use crate::types::Entry;
use crate::value::{field, field_str, normalize_last_wins};

const CODEX_TYPES: &[&str] = &[
    "session_meta",
    "response_item",
    "event_msg",
    "turn_context",
    "world_state",
    "compacted",
    "inter_agent_communication_metadata",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    Claude,
    Codex,
}

impl Provider {
    pub fn as_str(self) -> &'static str {
        match self {
            Provider::Claude => "claude",
            Provider::Codex => "codex",
        }
    }
}

#[derive(Debug)]
pub struct ParsedTranscript {
    pub provider: Provider,
    pub entries: Vec<Entry>,
    pub codex: Option<CodexSession>,
}

pub fn sniff_provider(bytes: &[u8]) -> Provider {
    let Some(line) = first_line(bytes) else {
        return Provider::Claude;
    };
    match sonic_rs::from_slice::<Value>(line) {
        Ok(mut value) => {
            normalize_last_wins(&mut value);
            let codex = field_str(&value, "type").is_some_and(|ty| CODEX_TYPES.contains(&ty))
                && field(&value, "payload").is_some();
            if codex {
                Provider::Codex
            } else {
                Provider::Claude
            }
        }
        Err(_) => Provider::Claude,
    }
}

pub fn parse_transcript_bytes(bytes: &[u8]) -> Result<ParsedTranscript, ParseError> {
    match sniff_provider(bytes) {
        Provider::Codex => {
            let session = parse_codex_bytes(bytes);
            Ok(ParsedTranscript {
                provider: Provider::Codex,
                entries: lower(&session).entries,
                codex: Some(session),
            })
        }
        Provider::Claude => Ok(ParsedTranscript {
            provider: Provider::Claude,
            entries: parse_bytes(bytes, |_| true)?,
            codex: None,
        }),
    }
}

pub fn transcript_session_activity(
    bytes: &[u8],
    opts: &ActivityOpts,
) -> Result<SessionActivity, ParseError> {
    match sniff_provider(bytes) {
        Provider::Codex => Ok(codex_session_activity(&parse_codex_bytes(bytes), opts)),
        Provider::Claude => Ok(session_activity(&parse_bytes(bytes, |_| true)?, opts)),
    }
}

fn first_line(bytes: &[u8]) -> Option<&[u8]> {
    let mut start = 0usize;
    for pos in memchr_iter(b'\n', bytes) {
        let line = &bytes[start..pos];
        if !line.iter().all(u8::is_ascii_whitespace) {
            return Some(line);
        }
        start = pos + 1;
    }
    let tail = &bytes[start..];
    (!tail.iter().all(u8::is_ascii_whitespace)).then_some(tail)
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::*;

    fn testdata_dir() -> PathBuf {
        Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../../tests/testdata/codex"
        ))
        .to_path_buf()
    }

    fn codex_bytes(tag: &str) -> Vec<u8> {
        let suffix = format!("{tag}.jsonl");
        let entry = std::fs::read_dir(testdata_dir())
            .expect("codex testdata dir")
            .filter_map(Result::ok)
            .find(|e| e.file_name().to_string_lossy().ends_with(&suffix))
            .unwrap_or_else(|| panic!("fixture {tag} not found"));
        std::fs::read(entry.path()).expect("read fixture")
    }

    const CC_USER: &str = r#"{"type":"user","uuid":"u","sessionId":"s1","timestamp":"2026-01-02T03:04:05Z","message":{"content":"run it"}}"#;
    const CC_SUMMARY: &str = r#"{"type":"summary","summary":"prior work","leafUuid":"x"}"#;

    #[test]
    fn sniff_codex_line() {
        assert_eq!(sniff_provider(&codex_bytes("050a")), Provider::Codex);
    }

    #[test]
    fn sniff_cc_user_line() {
        assert_eq!(sniff_provider(CC_USER.as_bytes()), Provider::Claude);
    }

    #[test]
    fn sniff_cc_summary_first_line() {
        let file = format!("{CC_SUMMARY}\n{CC_USER}\n");
        assert_eq!(sniff_provider(file.as_bytes()), Provider::Claude);
    }

    #[test]
    fn sniff_empty_is_claude() {
        assert_eq!(sniff_provider(b""), Provider::Claude);
        assert_eq!(sniff_provider(b"   \n\n"), Provider::Claude);
    }

    #[test]
    fn sniff_skips_leading_blank_lines() {
        let file = format!(
            "\n  \n{}\n",
            std::str::from_utf8(&codex_bytes("303")).unwrap()
        );
        assert_eq!(sniff_provider(file.as_bytes()), Provider::Codex);
    }

    #[test]
    fn sniff_type_without_payload_is_claude() {
        assert_eq!(
            sniff_provider(br#"{"type":"session_meta"}"#),
            Provider::Claude
        );
    }

    #[test]
    fn sniff_duplicate_type_uses_parser_last_wins() {
        let codex = br#"{"type":"summary","type":"session_meta","payload":{}}"#;
        assert_eq!(sniff_provider(codex), Provider::Codex);
        assert!(matches!(
            parse_codex_bytes(codex).entries[0].item,
            crate::codex::CodexItem::SessionMeta(_)
        ));

        let claude = br#"{"type":"session_meta","type":"summary","payload":{}}"#;
        assert_eq!(sniff_provider(claude), Provider::Claude);
        assert!(matches!(
            &parse_codex_bytes(claude).entries[0].item,
            crate::codex::CodexItem::Other(other) if other.ty.as_deref() == Some("summary")
        ));
    }

    #[test]
    fn parse_transcript_bytes_lowers_codex() {
        let parsed = parse_transcript_bytes(&codex_bytes("303")).unwrap();
        assert_eq!(parsed.provider, Provider::Codex);
        assert!(!parsed.entries.is_empty());
        assert!(parsed.codex.is_some());
    }

    #[test]
    fn parse_transcript_bytes_matches_parse_bytes_for_cc() {
        let file = format!("{CC_USER}\n");
        let parsed = parse_transcript_bytes(file.as_bytes()).unwrap();
        assert_eq!(parsed.provider, Provider::Claude);
        assert!(parsed.codex.is_none());
        let direct = parse_bytes(file.as_bytes(), |_| true).unwrap();
        assert_eq!(parsed.entries.len(), direct.len());
    }

    #[test]
    fn transcript_session_activity_maps_codex() {
        let activity =
            transcript_session_activity(&codex_bytes("050a"), &ActivityOpts::default()).unwrap();
        assert!(activity.mid_tool);
        assert!(!activity.is_waiting);
        assert_eq!(activity.pending.len(), 1);
        assert_eq!(activity.last_event_epoch, Some(1784220090));

        let completed =
            transcript_session_activity(&codex_bytes("050c"), &ActivityOpts::default()).unwrap();
        assert!(!completed.mid_tool);
    }
}
