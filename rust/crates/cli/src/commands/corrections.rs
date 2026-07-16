//! corrections_cli.py — the ledger's write/read surface, over the one native engine.

use cc_transcript_core::corrections::{Correction, CorrectionLog, SqlCell, SqlRow};
use cc_transcript_core::render::{pyjson_dumps, pyjson_loads, Json};

use crate::output::{click_error, usage_error, CliExit, Out};
use crate::target::home_dir;
use crate::CorrectionsCmd;

const QUERY_USAGE: &str = "cc-transcript corrections query [OPTIONS]";
const QUERY_HELP_PATH: &str = "cc-transcript corrections query";

const ROW_FIELDS: [&str; 16] = [
    "ts_ms",
    "session_id",
    "source",
    "anchor_uuid",
    "incorrect_digest",
    "incorrect_file",
    "incorrect_old",
    "incorrect_new",
    "correction_origin",
    "correction_file",
    "correction_old",
    "correction_new",
    "correction_commit",
    "correction_text",
    "overlap",
    "detail",
];

fn open_log() -> Result<CorrectionLog, CliExit> {
    let path = home_dir().join(".cc-transcript").join("corrections.db");
    CorrectionLog::open(&path).map_err(|e| click_error(&e.to_string()))
}

fn cell_json(name: &str, cell: &SqlCell) -> Result<Json, CliExit> {
    match cell {
        SqlCell::Null => Ok(Json::Null),
        SqlCell::Int(i) => Ok(Json::Int(*i)),
        SqlCell::Real(f) => Ok(Json::Float(*f)),
        SqlCell::Text(s) => Ok(Json::Str(s.clone())),
        SqlCell::Blob(_) => Err(click_error(&format!(
            "cannot serialize BLOB column {name:?} as JSON"
        ))),
    }
}

// corrections_cli.py emit_rows over asdict(Correction): fixed field order, the DB id
// dropped, detail re-parsed from its stored JSON text.
fn correction_json(row: &SqlRow) -> Result<Json, CliExit> {
    let by_name: std::collections::HashMap<&str, &SqlCell> = row
        .iter()
        .map(|(name, cell)| (name.as_str(), cell))
        .collect();
    let mut pairs: Vec<(String, Json)> = Vec::with_capacity(ROW_FIELDS.len());
    for field in ROW_FIELDS {
        let value = match field {
            "detail" => match by_name.get("detail_json") {
                Some(SqlCell::Text(text)) => pyjson_loads(text)
                    .map_err(|e| click_error(&format!("invalid detail JSON: {e}")))?,
                _ => Json::Null,
            },
            name => match by_name.get(name) {
                Some(cell) => cell_json(name, cell)?,
                None => Json::Null,
            },
        };
        pairs.push((field.to_string(), value));
    }
    Ok(Json::Obj(pairs))
}

fn emit_rows(rows: Vec<SqlRow>) -> Result<(), CliExit> {
    let mut out = Out::new();
    for row in &rows {
        out.line(&correction_json(row)?.dumps())?;
    }
    out.finish()
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

// corrections_cli.py add: detail = (json.loads(detail_json) if detail_json else {})
// | ({"repo": repo} if repo else {}); stored bytes are json.dumps defaults, matching
// the Python binding's storage contract (test_corrections_parity pins it).
fn merged_detail(detail_json: Option<&str>, repo: Option<&str>) -> Result<String, CliExit> {
    let mut pairs: Vec<(String, Json)> = match detail_json.filter(|text| !text.is_empty()) {
        Some(text) => {
            let parsed = pyjson_loads(text)
                .map_err(|e| click_error(&format!("invalid --detail JSON: {e}")))?;
            match parsed {
                Json::Obj(pairs) => pairs,
                _ => return Err(click_error("--detail must be a JSON object")),
            }
        }
        None => Vec::new(),
    };
    if let Some(repo) = repo.filter(|repo| !repo.is_empty()) {
        match pairs.iter_mut().find(|(key, _)| key == "repo") {
            Some(pair) => pair.1 = Json::Str(repo.to_string()),
            None => pairs.push(("repo".to_string(), Json::Str(repo.to_string()))),
        }
    }
    Ok(pyjson_dumps(&Json::Obj(pairs)))
}

pub fn run(cmd: CorrectionsCmd) -> Result<(), CliExit> {
    match cmd {
        CorrectionsCmd::Add {
            session,
            source,
            anchor,
            incorrect_file,
            ts_ms,
            origin,
            incorrect_old,
            incorrect_new,
            incorrect_digest,
            correction_file,
            correction_old,
            correction_new,
            correction_commit,
            correction_text,
            overlap,
            repo,
            detail_json,
        } => {
            let detail = merged_detail(detail_json.as_deref(), repo.as_deref())?;
            open_log()?
                .append(&Correction {
                    ts_ms: ts_ms.unwrap_or_else(now_ms),
                    session_id: session,
                    source,
                    anchor_uuid: anchor,
                    incorrect_digest: incorrect_digest.filter(|digest| !digest.is_empty()),
                    incorrect_file,
                    incorrect_old,
                    incorrect_new,
                    correction_origin: origin,
                    correction_file,
                    correction_old,
                    correction_new,
                    correction_commit,
                    correction_text,
                    overlap,
                    detail_json: detail,
                })
                .map_err(|e| click_error(&e.to_string()))
        }
        CorrectionsCmd::Query {
            session,
            repo,
            digest,
            since,
            source,
        } => {
            let log = open_log()?;
            let rows = match (&session, &digest, &repo, since) {
                (Some(s), Some(d), _, _) => log.by_digest(s, d),
                (Some(s), None, _, _) => log.for_session(s),
                (None, _, Some(r), _) => log.for_repo(r),
                (None, _, None, Some(ts)) => log.since(ts, source.as_deref()),
                _ => return Err(usage_error(
                    QUERY_USAGE,
                    QUERY_HELP_PATH,
                    "pass one of --session, --repo, or --since (and --digest only with --session)",
                )),
            }
            .map_err(|e| click_error(&e.to_string()))?;
            emit_rows(rows)
        }
        CorrectionsCmd::Sql { statement } => {
            let rows = open_log()?
                .sql(&statement)
                .map_err(|e| click_error(&e.to_string()))?;
            let mut out = Out::new();
            for row in &rows {
                // dict(sqlite3.Row) semantics: the first column wins a duplicate name.
                let mut pairs: Vec<(String, Json)> = Vec::with_capacity(row.len());
                for (name, cell) in row {
                    if pairs.iter().any(|(seen, _)| seen == name) {
                        continue;
                    }
                    pairs.push((name.clone(), cell_json(name, cell)?));
                }
                out.line(&Json::Obj(pairs).dumps())?;
            }
            out.finish()
        }
    }
}
