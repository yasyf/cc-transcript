//! cli.py `list`.

use cc_transcript_core::render::{display_path, human_size, Json};
use chrono::{DateTime, Local};

use crate::output::{click_error, CliExit, Out};
use crate::target::discover;
use crate::DiscoveryOpts;

fn local_minute(mtime: f64) -> String {
    let secs = mtime.floor();
    let nanos = ((mtime - secs) * 1e9) as u32;
    let utc = DateTime::from_timestamp(secs as i64, nanos).expect("mtime in range");
    utc.with_timezone(&Local)
        .format("%Y-%m-%d %H:%M")
        .to_string()
}

pub fn run(discovery: &DiscoveryOpts, json: bool) -> Result<(), CliExit> {
    let root = discovery.root();
    let matched = discover(
        &root,
        discovery.project.as_deref(),
        discovery.contains.as_deref(),
    );
    let shown: &[_] = if discovery.all {
        &matched
    } else {
        &matched[..matched.len().min(discovery.limit)]
    };
    let mut out = Out::new();
    if json {
        for (path, mtime) in shown {
            let size = std::fs::metadata(path)
                .map_err(|e| click_error(&format!("{}: {e}", path.display())))?
                .len();
            out.line(
                &Json::Obj(vec![
                    (
                        "path".into(),
                        Json::Str(path.to_string_lossy().into_owned()),
                    ),
                    ("mtime".into(), Json::Float(*mtime)),
                    ("size".into(), Json::UInt(size)),
                ])
                .dumps(),
            )?;
        }
        return out.finish();
    }
    for (path, mtime) in shown {
        let size = std::fs::metadata(path)
            .map_err(|e| click_error(&format!("{}: {e}", path.display())))?
            .len();
        out.line(&format!(
            "{} {:>8} {}",
            local_minute(*mtime),
            human_size(size),
            display_path(&path.to_string_lossy())
        ))?;
    }
    let count = if shown.len() == matched.len() {
        matched.len().to_string()
    } else {
        format!("{} of {}", shown.len(), matched.len())
    };
    out.line(&format!(
        "{count} transcripts under {}",
        display_path(&root.to_string_lossy())
    ))?;
    out.finish()
}
