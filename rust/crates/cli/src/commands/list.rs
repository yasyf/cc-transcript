//! cli.py `list`.

use std::path::{Path, PathBuf};

use cc_transcript_core::codex::discovery::{
    discover as discover_codex, sessions_root, RolloutFile,
};
use cc_transcript_core::discovery::mtime_secs;
use cc_transcript_core::render::{display_path, human_size, Json};
use chrono::{DateTime, Local};

use crate::output::{click_error, CliExit, Out};
use crate::target::{discover as discover_claude, require_dir};
use crate::DiscoveryOpts;

const USAGE: &str = "cc-transcript list [OPTIONS]";
const HELP_PATH: &str = "cc-transcript list";

enum ListRow {
    Claude {
        path: PathBuf,
        mtime: f64,
        size: u64,
    },
    Codex {
        rollout: RolloutFile,
        mtime: f64,
        size: u64,
    },
}

impl ListRow {
    fn mtime(&self) -> f64 {
        match self {
            ListRow::Claude { mtime, .. } | ListRow::Codex { mtime, .. } => *mtime,
        }
    }

    fn path(&self) -> &Path {
        match self {
            ListRow::Claude { path, .. } => path,
            ListRow::Codex { rollout, .. } => &rollout.path,
        }
    }

    fn json(&self) -> String {
        match self {
            ListRow::Claude { path, mtime, size } => Json::Obj(vec![
                (
                    "path".into(),
                    Json::Str(path.to_string_lossy().into_owned()),
                ),
                ("mtime".into(), Json::Float(*mtime)),
                ("size".into(), Json::UInt(*size)),
            ])
            .dumps(),
            ListRow::Codex {
                rollout,
                mtime,
                size,
            } => Json::Obj(vec![
                (
                    "path".into(),
                    Json::Str(rollout.path.to_string_lossy().into_owned()),
                ),
                ("mtime".into(), Json::Float(*mtime)),
                ("size".into(), Json::UInt(*size)),
                ("session_id".into(), Json::Str(rollout.session_id.clone())),
                ("compressed".into(), Json::Bool(rollout.compressed)),
            ])
            .dumps(),
        }
    }

    fn text(&self) -> String {
        match self {
            ListRow::Claude { path, mtime, size } => format!(
                "{} {:>8} {}",
                local_minute(*mtime),
                human_size(*size),
                display_path(&path.to_string_lossy())
            ),
            ListRow::Codex {
                rollout,
                mtime,
                size,
            } => {
                let marker = if rollout.compressed {
                    " [compressed]"
                } else {
                    ""
                };
                format!(
                    "{} {:>8} {} {}{}",
                    local_minute(*mtime),
                    human_size(*size),
                    rollout.session_id,
                    display_path(&rollout.path.to_string_lossy()),
                    marker,
                )
            }
        }
    }
}

fn local_minute(mtime: f64) -> String {
    let secs = mtime.floor();
    let nanos = ((mtime - secs) * 1e9) as u32;
    let utc = DateTime::from_timestamp(secs as i64, nanos).expect("mtime in range");
    utc.with_timezone(&Local)
        .format("%Y-%m-%d %H:%M")
        .to_string()
}

fn claude_rows(root: &Path, discovery: &DiscoveryOpts) -> Result<Vec<ListRow>, CliExit> {
    discover_claude(
        root,
        discovery.project.as_deref(),
        discovery.contains.as_deref(),
    )
    .into_iter()
    .map(|(path, mtime)| {
        let size = std::fs::metadata(&path)
            .map_err(|e| click_error(&format!("{}: {e}", path.display())))?
            .len();
        Ok(ListRow::Claude { path, mtime, size })
    })
    .collect()
}

fn codex_rows(root: &Path, contains: Option<&str>) -> Result<Vec<ListRow>, CliExit> {
    discover_codex(root)
        .into_iter()
        .filter(|rollout| {
            contains
                .filter(|needle| !needle.is_empty())
                .is_none_or(|needle| {
                    rollout
                        .path
                        .file_name()
                        .is_some_and(|name| name.to_string_lossy().contains(needle))
                })
        })
        .map(|rollout| {
            let metadata = std::fs::metadata(&rollout.path)
                .map_err(|e| click_error(&format!("{}: {e}", rollout.path.display())))?;
            Ok(ListRow::Codex {
                mtime: mtime_secs(&metadata),
                size: metadata.len(),
                rollout,
            })
        })
        .collect()
}

fn scope(provider: &str, claude_root: Option<&Path>, codex_root: Option<&Path>) -> String {
    match provider {
        "claude" => display_path(&claude_root.unwrap().to_string_lossy()),
        "codex" => display_path(&codex_root.unwrap().to_string_lossy()),
        "all" => format!(
            "{} and {}",
            display_path(&claude_root.unwrap().to_string_lossy()),
            display_path(&codex_root.unwrap().to_string_lossy()),
        ),
        _ => unreachable!("clap validates providers"),
    }
}

pub fn run(
    discovery: &DiscoveryOpts,
    provider: &str,
    codex_root: Option<&Path>,
    json: bool,
) -> Result<(), CliExit> {
    let (claude_root, codex_root) = match provider {
        "claude" => (Some(discovery.validated_root(USAGE, HELP_PATH)?), None),
        "codex" => {
            let root = sessions_root(codex_root);
            require_dir(&root, "'--codex-root'", USAGE, HELP_PATH)?;
            (None, Some(root))
        }
        "all" => {
            let claude_root = discovery.validated_root(USAGE, HELP_PATH)?;
            let codex_root = sessions_root(codex_root);
            require_dir(&codex_root, "'--codex-root'", USAGE, HELP_PATH)?;
            (Some(claude_root), Some(codex_root))
        }
        _ => unreachable!("clap validates providers"),
    };

    let mut rows = Vec::new();
    if let Some(root) = claude_root.as_deref() {
        rows.extend(claude_rows(root, discovery)?);
    }
    if let Some(root) = codex_root.as_deref() {
        rows.extend(codex_rows(root, discovery.contains.as_deref())?);
    }
    rows.sort_by(|a, b| {
        b.mtime()
            .partial_cmp(&a.mtime())
            .expect("mtimes are finite")
            .then_with(|| a.path().cmp(b.path()))
    });
    let total = rows.len();
    if let Some(limit) = discovery.effective_limit() {
        rows.truncate(limit);
    }

    let mut out = Out::new();
    if json {
        out.lines(rows.iter().map(ListRow::json))?;
        return out.finish();
    }
    out.lines(rows.iter().map(ListRow::text))?;
    let count = if rows.len() == total {
        total.to_string()
    } else {
        format!("{} of {total}", rows.len())
    };
    out.line(&format!(
        "{count} transcripts under {}",
        scope(provider, claude_root.as_deref(), codex_root.as_deref())
    ))?;
    out.finish()
}
