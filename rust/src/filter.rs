use std::collections::HashSet;

use regex::Regex;
use sonic_rs::{Index, JsonContainerTrait, JsonValueTrait, Value};

use crate::value::{content_text, field, field_bool, field_str, has_block_type};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    User,
    Assistant,
    System,
    Mode,
    Other,
}

pub struct CompiledSpec {
    clauses: Vec<CompiledClause>,
}

struct CompiledClause {
    predicate: CompiledPredicate,
    applies_to: Vec<Kind>,
    negate: bool,
}

enum CompiledPredicate {
    KindIs(Vec<Kind>),
    MetaFlag(&'static str),
    EntrypointIn(HashSet<String>),
    ModelIs(HashSet<String>),
    TextEmpty { consider_tool_use: bool },
    TextMatchesAny(Regex),
    TextInSet { phrases: HashSet<String>, strip_trailing: String },
    WordCountAtMost(usize),
}

fn parse_kind(name: &str) -> Result<Kind, String> {
    match name {
        "user" => Ok(Kind::User),
        "assistant" => Ok(Kind::Assistant),
        "system" => Ok(Kind::System),
        "mode" => Ok(Kind::Mode),
        "other" => Ok(Kind::Other),
        other => Err(format!("unknown event kind: {other}")),
    }
}

fn meta_json_key(flag: &str) -> Result<&'static str, String> {
    match flag {
        "is_sidechain" => Ok("isSidechain"),
        "is_meta" => Ok("isMeta"),
        "is_compact_summary" => Ok("isCompactSummary"),
        "is_visible_in_transcript_only" => Ok("isVisibleInTranscriptOnly"),
        other => Err(format!("unknown meta flag: {other}")),
    }
}

fn str_set(predicate: &Value, key: &str) -> HashSet<String> {
    field(predicate, key)
        .and_then(JsonContainerTrait::as_array)
        .into_iter()
        .flatten()
        .filter_map(JsonValueTrait::as_str)
        .map(String::from)
        .collect()
}

fn kind_array(predicate: &Value, key: &str) -> Result<Vec<Kind>, String> {
    field(predicate, key)
        .and_then(JsonContainerTrait::as_array)
        .into_iter()
        .flatten()
        .filter_map(JsonValueTrait::as_str)
        .map(parse_kind)
        .collect()
}

fn compile_regex(predicate: &Value) -> Result<Regex, String> {
    let groups = field(predicate, "groups")
        .and_then(JsonContainerTrait::as_array)
        .ok_or("TextMatchesAny missing 'groups'")?;
    compile_group_array(groups, field_bool(predicate, "ignore_case"))
}

/// Joins ``(name, pattern)`` group pairs into one alternation, matching the Python
/// ``compile_groups`` (so the score executor's regex stages share its semantics).
pub(crate) fn compile_group_array(groups: &sonic_rs::Array, ignore_case: bool) -> Result<Regex, String> {
    let joined = groups
        .iter()
        .filter_map(|group| 1usize.value_index_into(group).and_then(JsonValueTrait::as_str))
        .map(|pattern| format!("(?:{pattern})"))
        .collect::<Vec<_>>()
        .join("|");
    let pattern = if ignore_case { format!("(?i){joined}") } else { joined };
    Regex::new(&pattern).map_err(|e| format!("invalid regex {pattern:?}: {e}"))
}

fn compile_predicate(predicate: &Value) -> Result<CompiledPredicate, String> {
    match field_str(predicate, "kind").ok_or("predicate missing 'kind'")? {
        "KindIs" => Ok(CompiledPredicate::KindIs(kind_array(predicate, "kinds")?)),
        "MetaFlag" => Ok(CompiledPredicate::MetaFlag(meta_json_key(
            field_str(predicate, "flag").ok_or("MetaFlag missing 'flag'")?,
        )?)),
        "EntrypointIn" => Ok(CompiledPredicate::EntrypointIn(str_set(predicate, "entrypoints"))),
        "ModelIs" => Ok(CompiledPredicate::ModelIs(str_set(predicate, "models"))),
        "TextEmpty" => Ok(CompiledPredicate::TextEmpty {
            consider_tool_use: field_bool(predicate, "consider_tool_use"),
        }),
        "TextMatchesAny" => Ok(CompiledPredicate::TextMatchesAny(compile_regex(predicate)?)),
        "TextInSet" => Ok(CompiledPredicate::TextInSet {
            phrases: str_set(predicate, "phrases"),
            strip_trailing: field_str(predicate, "strip_trailing").unwrap_or("").to_string(),
        }),
        "WordCountAtMost" => Ok(CompiledPredicate::WordCountAtMost(
            field(predicate, "n")
                .and_then(JsonValueTrait::as_u64)
                .ok_or("WordCountAtMost missing 'n'")? as usize,
        )),
        other => Err(format!("unknown predicate kind: {other}")),
    }
}

fn compile_clause(clause: &Value) -> Result<CompiledClause, String> {
    Ok(CompiledClause {
        predicate: compile_predicate(field(clause, "predicate").ok_or("clause missing 'predicate'")?)?,
        applies_to: kind_array(clause, "applies_to")?,
        negate: field_bool(clause, "negate"),
    })
}

/// Compiles the JSON contract emitted by ``cc_transcript.filterspec.spec_to_json``.
/// Only ``DROP`` clauses are compiled; ``TAG`` clauses are applied Python-side.
pub fn compile_spec(spec_json: &str) -> Result<CompiledSpec, String> {
    let root: Value =
        sonic_rs::from_str(spec_json).map_err(|e| format!("invalid filter spec json: {e}"))?;
    let clauses = field(&root, "clauses")
        .and_then(JsonContainerTrait::as_array)
        .ok_or("spec missing 'clauses' array")?
        .iter()
        .filter(|clause| field_str(clause, "action") == Some("drop"))
        .map(compile_clause)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(CompiledSpec { clauses })
}

fn event_kind(data: &Value) -> Kind {
    match field_str(data, "type") {
        Some("user") => Kind::User,
        Some("assistant") => Kind::Assistant,
        Some("system") => Kind::System,
        Some("mode") | Some("permission-mode") => Kind::Mode,
        _ => Kind::Other,
    }
}

fn message_content<'a>(data: &'a Value) -> Option<&'a Value> {
    field(field(data, "message")?, "content")
}

fn message_str<'a>(data: &'a Value, key: &str) -> Option<&'a str> {
    field(data, "message").and_then(|message| field_str(message, key))
}

fn event_text(data: &Value, kind: Kind) -> String {
    match kind {
        Kind::User | Kind::Assistant => message_content(data).map(content_text).unwrap_or_default(),
        _ => String::new(),
    }
}

pub(crate) fn normalize_bare(text: &str, strip_trailing: &str) -> String {
    text.trim()
        .trim_end_matches(|c: char| strip_trailing.contains(c))
        .trim()
        .to_lowercase()
}

fn predicate_matches(predicate: &CompiledPredicate, data: &Value, kind: Kind, text: &str) -> bool {
    match predicate {
        CompiledPredicate::KindIs(kinds) => kinds.contains(&kind),
        CompiledPredicate::ModelIs(models) => {
            kind == Kind::Assistant && message_str(data, "model").is_some_and(|m| models.contains(m))
        }
        CompiledPredicate::TextEmpty { consider_tool_use } => match kind {
            Kind::Assistant if *consider_tool_use => {
                text.trim().is_empty()
                    && !message_content(data).is_some_and(|content| has_block_type(content, "tool_use"))
            }
            Kind::User | Kind::Assistant => text.trim().is_empty(),
            _ => false,
        },
        CompiledPredicate::TextMatchesAny(re) => re.is_match(text),
        CompiledPredicate::TextInSet { phrases, strip_trailing } => {
            phrases.contains(&normalize_bare(text, strip_trailing))
        }
        CompiledPredicate::WordCountAtMost(n) => text.split_whitespace().count() <= *n,
        CompiledPredicate::MetaFlag(json_key) => field_bool(data, json_key),
        CompiledPredicate::EntrypointIn(set) => field_str(data, "entrypoint").is_some_and(|e| set.contains(e)),
    }
}

fn clause_matches(clause: &CompiledClause, data: &Value, kind: Kind, text: &str) -> bool {
    if !clause.applies_to.is_empty() && !clause.applies_to.contains(&kind) {
        return false;
    }
    predicate_matches(&clause.predicate, data, kind, text) != clause.negate
}

/// Returns whether ``data`` survives every ``DROP`` clause of ``spec``.
pub fn spec_keep(spec: &CompiledSpec, data: &Value) -> bool {
    let kind = event_kind(data);
    let text = event_text(data, kind);
    !spec
        .clauses
        .iter()
        .any(|clause| clause_matches(clause, data, kind, &text))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(raw: &str) -> Value {
        sonic_rs::from_str(raw).unwrap()
    }

    fn pushback_spec() -> CompiledSpec {
        compile_spec(
            r#"{"clauses":[
                {"predicate":{"kind":"KindIs","kinds":["user"]},"action":"drop","applies_to":[],"negate":true,"label":null},
                {"predicate":{"kind":"MetaFlag","flag":"is_meta"},"action":"drop","applies_to":[],"negate":false,"label":null},
                {"predicate":{"kind":"TextMatchesAny","groups":[["interrupt","^\\s*\\[Request interrupted by user"]],"ignore_case":true},"action":"drop","applies_to":["user"],"negate":false,"label":null},
                {"predicate":{"kind":"TextInSet","phrases":["ok","go ahead"],"strip_trailing":".!?,;:"},"action":"drop","applies_to":["user"],"negate":false,"label":null},
                {"predicate":{"kind":"WordCountAtMost","n":2},"action":"drop","applies_to":["user"],"negate":false,"label":null}
            ]}"#,
        )
        .unwrap()
    }

    fn user(text: &str) -> Value {
        parse(&format!(
            r#"{{"type":"user","uuid":"u","sessionId":"s","timestamp":"2026-01-01T00:00:00Z","message":{{"role":"user","content":{}}}}}"#,
            sonic_rs::to_string(&text).unwrap()
        ))
    }

    #[test]
    fn keeps_real_pushback_drops_noise() {
        let spec = pushback_spec();
        assert!(spec_keep(&spec, &user("please refactor the parser carefully")));
        assert!(!spec_keep(&spec, &user("ok")));
        assert!(!spec_keep(&spec, &user("go ahead.")));
        assert!(!spec_keep(&spec, &user("fix it"))); // two words -> dropped
        assert!(spec_keep(&spec, &user("[Request interrupted by user] no, do it this way")) == false);
        assert!(spec_keep(&spec, &user("she quoted [Request interrupted by user] mid-text here"))); // head-anchored: mid-text marker kept
    }

    #[test]
    fn negate_kind_drops_non_users() {
        let spec = pushback_spec();
        let assistant = parse(
            r#"{"type":"assistant","uuid":"a","sessionId":"s","timestamp":"2026-01-01T00:00:00Z","message":{"model":"m","content":[{"type":"text","text":"hello there friend"}]}}"#,
        );
        assert!(!spec_keep(&spec, &assistant));
    }

    #[test]
    fn meta_flag_drops_meta_user() {
        let spec = pushback_spec();
        let meta_user = parse(
            r#"{"type":"user","uuid":"u","sessionId":"s","timestamp":"2026-01-01T00:00:00Z","isMeta":true,"message":{"role":"user","content":"a long real prompt here please"}}"#,
        );
        assert!(!spec_keep(&spec, &meta_user));
    }

    #[test]
    fn unknown_predicate_is_an_error() {
        assert!(compile_spec(r#"{"clauses":[{"predicate":{"kind":"Nope"},"action":"drop","applies_to":[],"negate":false}]}"#).is_err());
    }
}
