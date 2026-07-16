//! cli.py `show`, including the embedded SIGNAL/NOISE filter specs (cli.py
//! SIGNAL_SPEC and builders.py NOISE_SPEC, serialized via filterspec.spec_to_json;
//! the P6 literals inversion owns unifying these).

use std::path::PathBuf;

use cc_transcript_core::filter::{event_kind, spec_keep, CompiledSpec, NOISE_SPEC, SIGNAL_SPEC};
use cc_transcript_core::render::{compact_line, event_json};
use cc_transcript_core::types::Entry;

use crate::output::{eline, py_repr, usage_error, CliExit, Out};
use crate::target::{parse_single, py_path, require_files, tool_names};

const SHOW_CAP: usize = 200;
const USAGE: &str = "cc-transcript show [OPTIONS] PATH";
const HELP_PATH: &str = "cc-transcript show";

pub struct ShowArgs {
    pub path: PathBuf,
    pub head: Option<usize>,
    pub tail: Option<usize>,
    pub range: Option<String>,
    pub all: bool,
    pub kinds: Vec<String>,
    pub signal: bool,
    pub no_junk: bool,
    pub thinking: bool,
    pub width: usize,
    pub uuids: bool,
    pub json: bool,
}

type Bounds = (Option<i64>, Option<i64>);

fn parse_bounds(value: Option<&str>) -> Result<Option<Bounds>, CliExit> {
    let Some(value) = value else { return Ok(None) };
    let parts: Vec<&str> = value.split(':').collect();
    let invalid = || {
        usage_error(
            USAGE,
            HELP_PATH,
            &format!("invalid --range {}; expected A:B", py_repr(value)),
        )
    };
    match parts.as_slice() {
        [a, b] => {
            let side = |s: &str| -> Result<Option<i64>, CliExit> {
                if s.is_empty() {
                    return Ok(None);
                }
                s.trim().parse::<i64>().map(Some).map_err(|_| invalid())
            };
            Ok(Some((side(a)?, side(b)?)))
        }
        _ => Err(invalid()),
    }
}

pub fn filter_rows<'a>(
    entries: &'a [Entry],
    kinds: &[String],
    spec: Option<&CompiledSpec>,
) -> Vec<(usize, &'a Entry)> {
    entries
        .iter()
        .enumerate()
        .filter(|(_, event)| kinds.is_empty() || kinds.iter().any(|k| k == event_kind(event)))
        .filter(|(_, event)| spec.is_none_or(|s| spec_keep(s, event)))
        .collect()
}

fn slice_rows<'a>(
    rows: Vec<(usize, &'a Entry)>,
    head: Option<usize>,
    tail: Option<usize>,
    bounds: Option<Bounds>,
    all: bool,
) -> (Vec<(usize, &'a Entry)>, Option<String>) {
    match (head, tail, bounds) {
        (Some(n), _, _) => (rows.into_iter().take(n).collect(), None),
        (_, Some(0), _) => (Vec::new(), None),
        (_, Some(n), _) => {
            let skip = rows.len().saturating_sub(n);
            (rows.into_iter().skip(skip).collect(), None)
        }
        (_, _, Some((lo, hi))) => (
            rows.into_iter()
                .filter(|(i, _)| {
                    lo.is_none_or(|lo| *i as i64 >= lo) && hi.is_none_or(|hi| (*i as i64) < hi)
                })
                .collect(),
            None,
        ),
        _ if all || rows.len() <= SHOW_CAP => (rows, None),
        _ => {
            let hidden = rows.len() - SHOW_CAP;
            let skip = rows.len() - SHOW_CAP;
            (
                rows.into_iter().skip(skip).collect(),
                Some(format!(
                    "… {hidden} earlier events hidden — use --head/--range/--all"
                )),
            )
        }
    }
}

pub fn run(args: ShowArgs) -> Result<(), CliExit> {
    if [
        args.head.is_some(),
        args.tail.is_some(),
        args.range.is_some(),
    ]
    .iter()
    .filter(|set| **set)
    .count()
        > 1
    {
        return Err(usage_error(
            USAGE,
            HELP_PATH,
            "--head, --tail, and --range are mutually exclusive",
        ));
    }
    let bounds = parse_bounds(args.range.as_deref())?;
    let path = py_path(&args.path.to_string_lossy());
    require_files(std::slice::from_ref(&path), "'PATH'", USAGE, HELP_PATH)?;
    let parsed = parse_single(&path)?;
    let spec: Option<&CompiledSpec> = if args.signal {
        Some(&SIGNAL_SPEC)
    } else if args.no_junk {
        Some(&NOISE_SPEC)
    } else {
        None
    };
    let rows = filter_rows(&parsed.entries, &args.kinds, spec);
    let (selected, notice) = slice_rows(rows, args.head, args.tail, bounds, args.all);
    let mut out = Out::new();
    if args.json {
        if let Some(notice) = &notice {
            eline(notice);
        }
        for (index, event) in selected {
            out.line(&event_json(index, event).dumps())?;
        }
        return out.finish();
    }
    let names = tool_names(&parsed.entries);
    if let Some(notice) = &notice {
        out.line(notice)?;
    }
    for (index, event) in selected {
        out.line(&compact_line(
            index,
            event,
            &names,
            args.width,
            args.thinking,
            args.uuids,
        ))?;
    }
    out.finish()
}
