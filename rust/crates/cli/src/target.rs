//! Target discovery and parsing shared by every transcript command (cli.py's
//! discover / resolve_targets / parse_transcripts / scope_note plumbing).

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Condvar, Mutex};

use cc_transcript_core::discovery::mtime_secs;
use cc_transcript_core::gateway::parse_transcript_bytes;
use cc_transcript_core::parse::parse_timestamp;
use cc_transcript_core::render::display_path;
use cc_transcript_core::types::Entry;
use chrono::{DateTime, FixedOffset};
use sonic_rs::JsonValueTrait;

use crate::index::{transcripts, Transcript};
use crate::output::{click_error, eline, CliExit};

const WORKERS: usize = 8;
const AHEAD: usize = 4 * WORKERS;
const IN_FLIGHT_BYTES: u64 = 512 * 1024 * 1024;
const HEAD_BYTES: u64 = 256 * 1024;

// Python Path.home(): HOME env, then the pwd database (std::env::home_dir does both).
pub fn home_dir() -> PathBuf {
    std::env::home_dir().expect("a home directory resolves via HOME or the pwd database")
}

/// discovery.py CLAUDE_PROJECTS_DIR.
pub fn claude_projects_dir() -> PathBuf {
    home_dir().join(".claude").join("projects")
}

pub struct Targets {
    pub paths: Vec<(PathBuf, f64)>,
    pub total: usize,
}

/// Newest-first transcripts under `root` (cli.py discover), project and name
/// filtered, with ties in path order.
pub fn discover(root: &Path, project: Option<&str>, contains: Option<&str>) -> Vec<Transcript> {
    let mut found: Vec<Transcript> = transcripts(root)
        .into_iter()
        .filter(|transcript| {
            let relative = transcript
                .path
                .strip_prefix(root)
                .expect("indexed transcripts sit under their root");
            project.is_none_or(|project| {
                relative.parent().is_some_and(|parent| {
                    parent
                        .components()
                        .next()
                        .is_some_and(|first| first.as_os_str().to_string_lossy().contains(project))
                })
            }) && contains
                .filter(|needle| !needle.is_empty())
                .is_none_or(|needle| {
                    relative
                        .file_name()
                        .is_some_and(|name| name.to_string_lossy().contains(needle))
                })
        })
        .collect();
    found.sort_by(|a, b| {
        b.mtime
            .partial_cmp(&a.mtime)
            .expect("mtimes are finite")
            .then_with(|| {
                a.path
                    .as_os_str()
                    .as_encoded_bytes()
                    .cmp(b.path.as_os_str().as_encoded_bytes())
            })
    });
    found
}

pub fn resolve_targets(
    paths: &[PathBuf],
    root: &Path,
    project: Option<&str>,
    contains: Option<&str>,
    limit: Option<usize>,
    min_mtime: Option<f64>,
    until: Option<DateTime<FixedOffset>>,
) -> Result<Targets, CliExit> {
    let narrowed = |pairs| match until {
        Some(until) => started_before(pairs, until),
        None => Ok(pairs),
    };
    if !paths.is_empty() {
        let mut pairs = Vec::with_capacity(paths.len());
        for path in paths {
            let meta = std::fs::metadata(path)
                .map_err(|e| click_error(&format!("{}: {e}", path.display())))?;
            pairs.push((path.clone(), mtime_secs(&meta)));
        }
        let pairs = narrowed(pairs)?;
        let total = pairs.len();
        return Ok(Targets {
            paths: pairs,
            total,
        });
    }
    let matched: Vec<(PathBuf, f64)> = discover(root, project, contains)
        .into_iter()
        .map(|transcript| (transcript.path, transcript.mtime))
        .collect();
    let matched: Vec<(PathBuf, f64)> = match min_mtime {
        Some(floor) => matched
            .into_iter()
            .filter(|(_, mtime)| *mtime >= floor)
            .collect(),
        None => matched,
    };
    let matched = narrowed(matched)?;
    let total = matched.len();
    let paths = match limit {
        Some(n) => matched.into_iter().take(n).collect(),
        None => matched,
    };
    Ok(Targets { paths, total })
}

pub fn scope_note(targets: &Targets) -> Option<String> {
    (targets.paths.len() < targets.total).then(|| {
        format!(
            "searched {} of {} transcripts — use --all",
            targets.paths.len(),
            targets.total
        )
    })
}

pub struct Parsed {
    pub path: PathBuf,
    pub session_id: Option<String>,
    pub entries: Vec<Entry>,
}

// filterspec.session_id_of: the first meta-bearing entry names the session; the
// facts pipeline skips transcripts without one (facts.tool_facts).
fn session_id_of(entries: &[Entry]) -> Option<String> {
    entries
        .iter()
        .find_map(|entry| entry.meta().map(|meta| meta.session_id.clone()))
}

struct Claims {
    next: usize,
    next_weight: Option<u64>,
    emitted: usize,
    in_flight: u64,
    stopped: bool,
}

struct StopOnPanic<'a>(&'a Mutex<Claims>, &'a Condvar);

impl Drop for StopOnPanic<'_> {
    fn drop(&mut self) {
        if std::thread::panicking() {
            self.0.lock().expect("pool claims").stopped = true;
            self.1.notify_all();
        }
    }
}

/// Run `work` over `0..len` on [`WORKERS`] threads and hand each result to `sink` in
/// index order on the calling thread. A worker stays within [`AHEAD`] items of the last
/// one emitted and within [`IN_FLIGHT_BYTES`] of in-flight `weight`, except that an item
/// always runs once nothing else is in flight, so a slow sink or a run of huge items
/// holds the workers back instead of growing memory.
fn ordered_pool<T: Send>(
    len: usize,
    weight: impl Fn(usize) -> u64 + Sync,
    work: impl Fn(usize) -> T + Sync,
    mut sink: impl FnMut(T) -> Result<(), CliExit>,
) -> Result<(), CliExit> {
    let claims = Mutex::new(Claims {
        next: 0,
        next_weight: None,
        emitted: 0,
        in_flight: 0,
        stopped: false,
    });
    let turn = Condvar::new();
    let (sent, received) = mpsc::channel();
    std::thread::scope(|scope| {
        for _ in 0..WORKERS.min(len) {
            let sent = sent.clone();
            let (claims, turn, weight, work) = (&claims, &turn, &weight, &work);
            scope.spawn(move || {
                let _stop = StopOnPanic(claims, turn);
                let mut held = claims.lock().expect("pool claims");
                while !held.stopped && held.next < len {
                    let next = held.next;
                    let cost = *held.next_weight.get_or_insert_with(|| weight(next));
                    if next >= held.emitted + AHEAD
                        || held.in_flight > 0 && held.in_flight + cost > IN_FLIGHT_BYTES
                    {
                        held = turn.wait(held).expect("pool claims");
                        continue;
                    }
                    held.next += 1;
                    held.next_weight = None;
                    held.in_flight += cost;
                    drop(held);
                    let value = work(next);
                    held = claims.lock().expect("pool claims");
                    held.in_flight -= cost;
                    turn.notify_all();
                    if sent.send((next, value)).is_err() {
                        return;
                    }
                }
            });
        }
        drop(sent);
        let mut pending = HashMap::new();
        let mut emitted = 0;
        for (index, value) in received {
            pending.insert(index, value);
            while let Some(value) = pending.remove(&emitted) {
                let result = sink(value);
                emitted += 1;
                let mut held = claims.lock().expect("pool claims");
                held.emitted = emitted;
                held.stopped |= result.is_err();
                drop(held);
                turn.notify_all();
                result?;
            }
        }
        Ok(())
    })
}

/// Parse `targets` on a bounded pool, reducing each transcript on its worker so its
/// events are gone before the next one starts, and hand the results to `sink` in target
/// order as they finish. Warns once at the end about unparseable files. Memory stays
/// flat in the size of the sweep as long as `sink` folds or writes what it receives.
pub fn stream_transcripts<T: Send>(
    targets: &[(PathBuf, f64)],
    reduce: impl Fn(Parsed) -> T + Sync,
    mut sink: impl FnMut(T) -> Result<(), CliExit>,
) -> Result<(), CliExit> {
    let mut missing: Vec<String> = Vec::new();
    let mut index = 0;
    ordered_pool(
        targets.len(),
        |index| std::fs::metadata(&targets[index].0).map_or(0, |meta| meta.len()),
        |index| {
            let path = &targets[index].0;
            let bytes = std::fs::read(path).ok()?;
            let entries = parse_transcript_bytes(&bytes).ok()?.entries;
            drop(bytes);
            Some(reduce(Parsed {
                path: path.clone(),
                session_id: session_id_of(&entries),
                entries,
            }))
        },
        |value| {
            let path = &targets[index].0;
            index += 1;
            match value {
                Some(value) => sink(value),
                None => {
                    missing.push(display_path(&path.to_string_lossy()));
                    Ok(())
                }
            }
        },
    )?;
    warn_unparseable(&missing);
    Ok(())
}

pub fn warn_unparseable(skipped: &[String]) {
    if !skipped.is_empty() {
        eline(&format!(
            "warning: skipped {} unparseable transcript(s): {}",
            skipped.len(),
            skipped.join(", ")
        ));
    }
}

/// Parse every target and keep its events, in target order (cli.py parse_transcripts).
/// For a known handful of files; a sweep goes through [`stream_transcripts`].
pub fn parse_transcripts(targets: &[(PathBuf, f64)]) -> Vec<Parsed> {
    let mut parsed = Vec::with_capacity(targets.len());
    stream_transcripts(
        targets,
        |transcript| transcript,
        |transcript| {
            parsed.push(transcript);
            Ok(())
        },
    )
    .expect("collecting parsed transcripts cannot fail");
    parsed
}

/// The timestamp of the first user, assistant, or Codex rollout event, when one sits
/// within [`HEAD_BYTES`] of the start of the file. A transcript only appends, so
/// nothing in it is older than this.
fn first_event(path: &Path) -> Option<DateTime<FixedOffset>> {
    let mut head = BufReader::new(File::open(path).ok()?.take(HEAD_BYTES));
    let mut line = Vec::new();
    loop {
        line.clear();
        if head.read_until(b'\n', &mut line).ok()? == 0 || line.last() != Some(&b'\n') {
            return None;
        }
        let kind = sonic_rs::get_from_slice(&line, &["type"]).ok();
        if kind
            .as_ref()
            .and_then(|kind| kind.as_str())
            .is_some_and(|kind| {
                matches!(
                    kind,
                    "user"
                        | "assistant"
                        | "session_meta"
                        | "response_item"
                        | "event_msg"
                        | "turn_context"
                )
            })
        {
            let stamp = sonic_rs::get_from_slice(&line, &["timestamp"]).ok()?;
            return parse_timestamp(stamp.as_str()?).ok();
        }
    }
}

/// Drop the targets that start at or after `until`, reading only the head of each. A
/// file whose first event cannot be read from its head stays.
pub fn started_before(
    targets: Vec<(PathBuf, f64)>,
    until: DateTime<FixedOffset>,
) -> Result<Vec<(PathBuf, f64)>, CliExit> {
    let mut kept = Vec::new();
    let mut index = 0;
    ordered_pool(
        targets.len(),
        |_| 0,
        |index| first_event(&targets[index].0).is_none_or(|first| first < until),
        |keep| {
            if keep {
                kept.push(targets[index].clone());
            }
            index += 1;
            Ok(())
        },
    )?;
    Ok(kept)
}

/// cli.py parse_single: one path or a click-style `Error:` exit 1.
pub fn parse_single(path: &Path) -> Result<Parsed, CliExit> {
    let mtime = std::fs::metadata(path)
        .map(|m| mtime_secs(&m))
        .unwrap_or(0.0);
    parse_transcripts(&[(path.to_path_buf(), mtime)])
        .into_iter()
        .next()
        .ok_or_else(|| {
            click_error(&format!(
                "failed to parse {}",
                display_path(&path.to_string_lossy())
            ))
        })
}

/// click `Path(file_okay=False)`: an existing non-directory root exits 2.
pub fn require_dir(path: &Path, hint: &str, usage: &str, help_path: &str) -> Result<(), CliExit> {
    if path.exists() && !path.is_dir() {
        return Err(crate::output::usage_error(
            usage,
            help_path,
            &format!(
                "Invalid value for {hint}: Directory {} is a file.",
                crate::output::py_repr(&path.to_string_lossy())
            ),
        ));
    }
    Ok(())
}

/// click `Path(exists=True, dir_okay=False)`: missing or directory args exit 2 with a
/// click-shaped invalid-value message.
pub fn require_files(
    paths: &[PathBuf],
    hint: &str,
    usage: &str,
    help_path: &str,
) -> Result<(), CliExit> {
    for path in paths {
        let display = path.to_string_lossy();
        let message = if !path.exists() {
            format!(
                "Invalid value for {hint}: File {} does not exist.",
                crate::output::py_repr(&display)
            )
        } else if path.is_dir() {
            format!(
                "Invalid value for {hint}: File {} is a directory.",
                crate::output::py_repr(&display)
            )
        } else {
            continue;
        };
        return Err(crate::output::usage_error(usage, help_path, &message));
    }
    Ok(())
}

/// filterspec.py tool_names: every ToolUseBlock's id → name across `entries`.
pub fn tool_names(entries: &[Entry]) -> std::collections::HashMap<&str, &str> {
    entries
        .iter()
        .flat_map(|entry| entry.tool_uses())
        .map(|tu| (tu.id.as_str(), tu.name.as_str()))
        .collect()
}

/// Python `str(Path(arg))`: collapse duplicate separators and `.` segments.
pub fn py_path(arg: &str) -> PathBuf {
    if arg.is_empty() {
        return PathBuf::from(".");
    }
    let absolute = arg.starts_with('/');
    let parts: Vec<&str> = arg
        .split('/')
        .filter(|p| !p.is_empty() && *p != ".")
        .collect();
    let joined = parts.join("/");
    let text = match (absolute, joined.is_empty()) {
        (true, true) => "/".to_string(),
        (true, false) => format!("/{joined}"),
        (false, true) => ".".to_string(),
        (false, false) => joined,
    };
    PathBuf::from(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pool_emits_in_order_and_never_runs_past_its_window() {
        let furthest = std::sync::atomic::AtomicUsize::new(0);
        let mut seen = Vec::new();
        ordered_pool(
            200,
            |_| 0,
            |index| {
                furthest.fetch_max(index, std::sync::atomic::Ordering::SeqCst);
                index
            },
            |index| {
                assert!(furthest.load(std::sync::atomic::Ordering::SeqCst) < index + 1 + AHEAD);
                seen.push(index);
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(seen, (0..200).collect::<Vec<_>>());
    }

    #[test]
    fn pool_holds_in_flight_weight_to_its_budget_except_for_a_lone_item() {
        let in_flight = std::sync::atomic::AtomicU64::new(0);
        let peak = std::sync::atomic::AtomicU64::new(0);
        let ordering = std::sync::atomic::Ordering::SeqCst;
        let cost = |index: usize| match index % 4 {
            0 => 2 * IN_FLIGHT_BYTES,
            _ => IN_FLIGHT_BYTES / 3,
        };
        ordered_pool(
            64,
            cost,
            |index| {
                let now = in_flight.fetch_add(cost(index), ordering) + cost(index);
                if cost(index) <= IN_FLIGHT_BYTES {
                    peak.fetch_max(now, ordering);
                } else {
                    assert_eq!(now, cost(index));
                }
                std::thread::sleep(std::time::Duration::from_millis(2));
                in_flight.fetch_sub(cost(index), ordering);
            },
            |()| Ok(()),
        )
        .unwrap();
        assert!(peak.load(ordering) <= IN_FLIGHT_BYTES);
    }

    #[test]
    fn pool_stops_claiming_once_the_sink_fails() {
        let ran = std::sync::atomic::AtomicUsize::new(0);
        let result = ordered_pool(
            10_000,
            |_| 0,
            |_| {
                ran.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            },
            |()| Err(CliExit(0)),
        );
        assert!(matches!(result, Err(CliExit(0))));
        assert!(ran.load(std::sync::atomic::Ordering::SeqCst) <= 1 + AHEAD);
    }

    #[test]
    fn project_discovery_only_walks_matching_roots() {
        let root = tempfile::tempdir().unwrap();
        let wanted = root.path().join("wanted-project");
        let other = root.path().join("other-project");
        std::fs::create_dir(&wanted).unwrap();
        std::fs::create_dir(&other).unwrap();
        std::fs::write(wanted.join("match.jsonl"), "{}").unwrap();
        std::fs::write(other.join("match.jsonl"), "{}").unwrap();

        let found = discover(root.path(), Some("wanted"), None);

        assert_eq!(found.len(), 1);
        assert_eq!(found[0].path, wanted.join("match.jsonl"));
    }
}
