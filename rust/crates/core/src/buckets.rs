//! Sentiment event bucketing — grouping conversational events into per-session,
//! time-aligned windows worth scoring. Ported from `cc_transcript/sentiment/buckets.py`
//! (ConversationBucketer.bucket_events).

use std::collections::{BTreeMap, HashMap};

use chrono::{DateTime, Duration, FixedOffset, Timelike};
use once_cell::sync::Lazy;
use regex::Regex;

use crate::literals::protocol::SENTIMENT_JUNK_PATTERN;
use crate::pystr::strip;
use crate::types::Entry;

const BUCKET_MINUTES: u32 = 3;
const MIN_USER_TURNS_PER_SESSION: usize = 2;
const MIN_USER_CHARS: usize = 5;

/// JUNK_USER_MESSAGE_RE (filterspec.py): the protocol-noise alternation dropped from
/// user turns before bucketing, compiled case-insensitively to match Python's
/// `compile_groups(SENTIMENT_JUNK_GROUPS, True)`. The pattern's `\s`/`\b`/`\w` classes
/// carry the accepted, theoretical-only engine divergences documented in cc-notes
/// d458ca8 — never triggered by real transcripts, and the same pattern already runs
/// the Rust filter path. The golden pins the realistic junk alternatives.
static SENTIMENT_JUNK_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(&format!("(?i){SENTIMENT_JUNK_PATTERN}")).expect("sentiment junk regex")
});

/// A session's conversational events grouped into one fixed-width time window
/// (buckets.py ConversationBucket) — the unit that gets scored.
pub struct ConversationBucket<'a> {
    pub session_id: &'a str,
    pub bucket_index: i64,
    pub bucket_start: DateTime<FixedOffset>,
    pub events: Vec<&'a Entry>,
}

/// align_to_bucket (buckets.py): floor `ts` to the head of its `BUCKET_MINUTES` window.
fn align_to_bucket(ts: DateTime<FixedOffset>) -> DateTime<FixedOffset> {
    ts.with_minute(ts.minute() / BUCKET_MINUTES * BUCKET_MINUTES)
        .unwrap()
        .with_second(0)
        .unwrap()
        .with_nanosecond(0)
        .unwrap()
}

/// The window index of `ts` relative to `session_start`, mirroring Python's
/// `int((ts - session_start) // timedelta(minutes=BUCKET_MINUTES))` at microsecond
/// resolution. `ts >= session_start` always holds (session_start floors the earliest
/// event), so integer truncation equals floor.
fn bucket_index(ts: DateTime<FixedOffset>, session_start: DateTime<FixedOffset>) -> i64 {
    (ts - session_start)
        .num_microseconds()
        .expect("session span fits i64 microseconds")
        / (BUCKET_MINUTES as i64 * 60_000_000)
}

fn timestamp(entry: &Entry) -> DateTime<FixedOffset> {
    entry
        .meta()
        .expect("user/assistant entries carry meta")
        .timestamp
}

fn is_substantive_user(entry: &Entry) -> bool {
    matches!(entry, Entry::User(user) if strip(&user.content.text()).chars().count() >= MIN_USER_CHARS)
}

/// bucket_events (buckets.py ConversationBucketer.bucket_events): lifts the
/// conversational events in `entries` into scorable per-session, time-aligned windows.
///
/// User turns matching the sentiment junk regex are dropped before grouping; sessions
/// below `MIN_USER_TURNS_PER_SESSION` and windows lacking a substantive user turn or
/// any assistant turn are dropped. Sessions are emitted in first-appearance order to
/// match the Python dict's insertion order.
pub fn bucket_events(entries: &[Entry]) -> Vec<ConversationBucket<'_>> {
    bucket_events_refs(&entries.iter().collect::<Vec<_>>())
}

/// `bucket_events` over borrowed entry views — the events-in native path, where each
/// view borrows its `&Entry` behind the shared parse buffer (no re-parse). `bucket_events`
/// delegates here after collecting its owned entries into refs.
pub fn bucket_events_refs<'a>(entries: &[&'a Entry]) -> Vec<ConversationBucket<'a>> {
    let mut order: Vec<&'a str> = Vec::new();
    let mut by_session: HashMap<&'a str, Vec<&'a Entry>> = HashMap::new();
    for &entry in entries {
        let session_id = match entry {
            Entry::User(user) => {
                if SENTIMENT_JUNK_RE.is_match(&user.content.text()) {
                    continue;
                }
                user.meta.session_id.as_str()
            }
            Entry::Assistant(assistant) => assistant.meta.session_id.as_str(),
            _ => continue,
        };
        if !by_session.contains_key(session_id) {
            order.push(session_id);
        }
        by_session.entry(session_id).or_default().push(entry);
    }

    let mut buckets = Vec::new();
    for session_id in order {
        let mut session_events = by_session
            .remove(session_id)
            .expect("session_id from order");
        if session_events
            .iter()
            .filter(|e| matches!(e, Entry::User(_)))
            .count()
            < MIN_USER_TURNS_PER_SESSION
        {
            continue;
        }
        session_events.sort_by_key(|e| timestamp(e));
        let session_start = align_to_bucket(timestamp(session_events[0]));

        let mut grouped: BTreeMap<i64, Vec<&Entry>> = BTreeMap::new();
        for entry in session_events {
            grouped
                .entry(bucket_index(timestamp(entry), session_start))
                .or_default()
                .push(entry);
        }
        for (idx, window) in grouped {
            if !window.iter().any(|e| is_substantive_user(e))
                || !window.iter().any(|e| matches!(e, Entry::Assistant(_)))
            {
                continue;
            }
            buckets.push(ConversationBucket {
                session_id,
                bucket_index: idx,
                bucket_start: session_start + Duration::minutes(BUCKET_MINUTES as i64 * idx),
                events: window,
            });
        }
    }
    buckets
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::parse_bytes;

    fn parse(lines: &[String]) -> Vec<Entry> {
        parse_bytes(lines.join("\n").as_bytes(), |_| true).unwrap()
    }

    fn user(session: &str, ts: &str, text: &str) -> String {
        format!(
            r#"{{"type":"user","uuid":"{ts}","sessionId":"{session}","timestamp":"{ts}","message":{{"role":"user","content":"{text}"}}}}"#
        )
    }

    fn assistant(session: &str, ts: &str) -> String {
        format!(
            r#"{{"type":"assistant","uuid":"a-{ts}","sessionId":"{session}","timestamp":"{ts}","message":{{"role":"assistant","model":"m","content":[{{"type":"text","text":"working"}}]}}}}"#
        )
    }

    #[test]
    fn one_window_needs_a_substantive_user_and_an_assistant() {
        let entries = parse(&[
            user("s", "2026-01-06T09:01:00.000Z", "please fix the parser"),
            assistant("s", "2026-01-06T09:01:30.000Z"),
            user("s", "2026-01-06T09:02:00.000Z", "and the tests"),
        ]);
        let buckets = bucket_events(&entries);
        assert_eq!(buckets.len(), 1);
        assert_eq!(buckets[0].bucket_index, 0);
        // 09:01 floors to 09:00 → bucket_start is 09:00Z.
        assert_eq!(buckets[0].bucket_start.timestamp(), 1_767_690_000);
        assert_eq!(buckets[0].events.len(), 3);
    }

    #[test]
    fn junk_user_turns_are_dropped_and_do_not_count() {
        // Two real user turns plus an interrupt marker; the interrupt is junk, so the
        // session still has two counting user turns and one bucket.
        let entries = parse(&[
            user("s", "2026-01-06T09:00:00.000Z", "fix the bug please"),
            user(
                "s",
                "2026-01-06T09:00:10.000Z",
                "[Request interrupted by user]",
            ),
            assistant("s", "2026-01-06T09:00:20.000Z"),
            user("s", "2026-01-06T09:00:30.000Z", "handle the sidechain too"),
        ]);
        let buckets = bucket_events(&entries);
        assert_eq!(buckets.len(), 1);
        assert!(buckets[0]
            .events
            .iter()
            .all(|e| !matches!(e, Entry::User(u) if u.content.text().contains("interrupted"))));
    }

    #[test]
    fn session_below_min_user_turns_is_dropped() {
        let entries = parse(&[
            user("s", "2026-01-06T09:00:00.000Z", "just one real prompt"),
            assistant("s", "2026-01-06T09:00:10.000Z"),
        ]);
        assert!(bucket_events(&entries).is_empty());
    }

    #[test]
    fn short_user_window_without_substance_is_dropped() {
        // Both user turns are short acks (<5 chars) — no substantive user, so no bucket.
        let entries = parse(&[
            user("s", "2026-01-06T09:00:00.000Z", "ok"),
            user("s", "2026-01-06T09:00:10.000Z", "yes"),
            assistant("s", "2026-01-06T09:00:20.000Z"),
        ]);
        assert!(bucket_events(&entries).is_empty());
    }

    #[test]
    fn time_gap_splits_into_separate_indices() {
        let entries = parse(&[
            user("s", "2026-01-06T09:00:00.000Z", "first substantive prompt"),
            assistant("s", "2026-01-06T09:00:30.000Z"),
            user("s", "2026-01-06T09:10:00.000Z", "second substantive prompt"),
            assistant("s", "2026-01-06T09:10:30.000Z"),
        ]);
        let buckets = bucket_events(&entries);
        assert_eq!(buckets.len(), 2);
        assert_eq!(buckets[0].bucket_index, 0);
        assert_eq!(buckets[1].bucket_index, 3); // 10 minutes // 3 = 3
    }

    #[test]
    fn sessions_emit_in_first_appearance_order() {
        let entries = parse(&[
            user("b", "2026-01-06T09:00:00.000Z", "session b first prompt"),
            user("a", "2026-01-06T09:00:05.000Z", "session a first prompt"),
            assistant("b", "2026-01-06T09:00:10.000Z"),
            assistant("a", "2026-01-06T09:00:15.000Z"),
            user("b", "2026-01-06T09:00:20.000Z", "session b second prompt"),
            user("a", "2026-01-06T09:00:25.000Z", "session a second prompt"),
        ]);
        let buckets = bucket_events(&entries);
        assert_eq!(buckets.len(), 2);
        assert_eq!(buckets[0].session_id, "b");
        assert_eq!(buckets[1].session_id, "a");
    }

    #[test]
    fn sub_microsecond_ties_keep_input_order() {
        // Same µs, differing sub-µs digits: µs-truncation (parse.rs) makes them tie, and the
        // stable sort keeps input order — without truncation the 100ns assistant sorts first.
        let entries = parse(&[
            user(
                "s",
                "2026-01-06T09:00:00.000000900Z",
                "substantive prompt one",
            ),
            assistant("s", "2026-01-06T09:00:00.000000100Z"),
            user(
                "s",
                "2026-01-06T09:00:01.000000000Z",
                "substantive prompt two",
            ),
        ]);
        let buckets = bucket_events(&entries);
        assert_eq!(buckets.len(), 1);
        let uuids: Vec<&str> = buckets[0]
            .events
            .iter()
            .map(|e| e.meta().unwrap().uuid.as_str())
            .collect();
        assert_eq!(
            uuids,
            vec![
                "2026-01-06T09:00:00.000000900Z",
                "a-2026-01-06T09:00:00.000000100Z",
                "2026-01-06T09:00:01.000000000Z",
            ]
        );
    }

    #[test]
    fn exact_three_minute_boundary_starts_new_index() {
        // 09:00:00 aligns to idx 0; exactly +180s crosses to idx 1, one tick short stays at 0.
        let entries = parse(&[
            user("s", "2026-01-06T09:00:00.000Z", "first substantive prompt"),
            assistant("s", "2026-01-06T09:02:59.999Z"),
            user("s", "2026-01-06T09:03:00.000Z", "second substantive prompt"),
            assistant("s", "2026-01-06T09:03:00.001Z"),
        ]);
        let buckets = bucket_events(&entries);
        assert_eq!(
            buckets.iter().map(|b| b.bucket_index).collect::<Vec<_>>(),
            vec![0, 1]
        );
    }

    #[test]
    fn bucket_events_refs_matches_the_owned_slice_path() {
        let entries = parse(&[
            user("s", "2026-01-06T09:01:00.000Z", "please fix the parser"),
            assistant("s", "2026-01-06T09:01:30.000Z"),
            user("s", "2026-01-06T09:02:00.000Z", "and the tests"),
        ]);
        let refs: Vec<&Entry> = entries.iter().collect();
        let via_refs = bucket_events_refs(&refs);
        let via_owned = bucket_events(&entries);
        assert_eq!(via_refs.len(), via_owned.len());
        let project = |buckets: &[ConversationBucket]| {
            buckets
                .iter()
                .map(|b| {
                    (
                        b.session_id.to_owned(),
                        b.bucket_index,
                        b.bucket_start.timestamp_millis(),
                        b.events
                            .iter()
                            .map(|e| e.meta().unwrap().uuid.to_string())
                            .collect::<Vec<_>>(),
                    )
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(project(&via_refs), project(&via_owned));
    }
}
