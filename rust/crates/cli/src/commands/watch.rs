//! cli.py `watch` — the live tailer over the native watch core.

use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::time::Duration;

use cc_transcript_core::filter::event_kind;
use cc_transcript_core::render::{event_payload, truncate, Json};
use cc_transcript_core::watch::{tick, TailState, WatchEvent};

use crate::output::{CliExit, Out};
use crate::target::claude_projects_dir;
use crate::{WATCH_ACTIVE, WATCH_INTERRUPTED};

const BLANK_TIME: &str = "        ";

fn tag_for(kind: &str) -> &'static str {
    match kind {
        "user" => "user",
        "assistant" => "asst",
        "system" => "sys",
        "mode" => "mode",
        "other" => "other",
        "attachment" => "att",
        _ => unreachable!("event_kind yields one of the six tags"),
    }
}

fn char_prefix(s: &str, n: usize) -> &str {
    match s.char_indices().nth(n) {
        Some((idx, _)) => &s[..idx],
        None => s,
    }
}

fn watch_line(item: &WatchEvent) -> String {
    let meta = item.event.meta();
    let time = meta.map_or(BLANK_TIME.to_string(), |m| {
        m.timestamp.format("%H:%M:%S").to_string()
    });
    let tag = format!(
        "{}{}",
        tag_for(event_kind(&item.event)),
        if item.is_sidechain { "*" } else { "" }
    );
    let payload = event_payload(&item.event, &Default::default(), 100, false);
    cc_transcript_core::pystr::rstrip(&format!(
        "{time} {} {tag:<5} {payload}",
        char_prefix(&item.session_id, 8)
    ))
    .to_string()
}

fn watch_json(item: &WatchEvent) -> String {
    let meta = item.event.meta();
    let kind = event_kind(&item.event);
    Json::Obj(vec![
        (
            "path".into(),
            Json::Str(item.path.to_string_lossy().into_owned()),
        ),
        ("session_id".into(), Json::Str(item.session_id.clone())),
        ("is_sidechain".into(), Json::Bool(item.is_sidechain)),
        (
            "uuid".into(),
            meta.map_or(Json::Null, |m| Json::Str(m.uuid.clone())),
        ),
        ("kind".into(), Json::Str(kind.to_string())),
        (
            "role".into(),
            if kind == "user" || kind == "assistant" {
                Json::Str(kind.to_string())
            } else {
                Json::Null
            },
        ),
        (
            "preview".into(),
            Json::Str(truncate(
                &event_payload(&item.event, &Default::default(), 120, false),
                120,
            )),
        ),
    ])
    .dumps()
}

pub fn run(roots: &[PathBuf], poll: f64, from_start: bool, json: bool) -> Result<(), CliExit> {
    let roots: Vec<PathBuf> = if roots.is_empty() {
        vec![claude_projects_dir()]
    } else {
        roots.to_vec()
    };
    let mut state = TailState::default();
    let mut out = Out::new();
    WATCH_ACTIVE.store(true, Ordering::SeqCst);
    loop {
        if WATCH_INTERRUPTED.load(Ordering::SeqCst) {
            return Ok(());
        }
        for item in tick(&mut state, &roots, from_start) {
            out.line(&if json {
                watch_json(&item)
            } else {
                watch_line(&item)
            })?;
        }
        out.finish()?;
        let mut remaining = poll;
        while remaining > 0.0 {
            if WATCH_INTERRUPTED.load(Ordering::SeqCst) {
                return Ok(());
            }
            let step = remaining.min(0.05);
            std::thread::sleep(Duration::from_secs_f64(step));
            remaining -= step;
        }
    }
}
