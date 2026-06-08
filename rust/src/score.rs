use std::collections::HashSet;

use regex::Regex;
use sonic_rs::{JsonContainerTrait, JsonValueTrait, Value};

use crate::filter::{compile_group_array, normalize_bare};
use crate::lexicon;
use crate::value::{field, field_bool, field_str};

// Compiles the JSON contract emitted by cc_transcript.sentiment.scorespec.score_spec_to_json
// and evaluates it with the same semantics as the Python interpreter. The LLM stays
// Python-side: these run before (short-circuit) and after (post-process) inference.

enum ShortStage {
    Frustration { re: Regex, score: i64 },
}

enum PostStage {
    PositiveClamp { from: i64, to: i64, max_words: usize, floor: i32 },
    MildIrritation { trigger: Regex, hostile: Regex, from: i64, to: i64, floor: i32 },
    ResumeClamp { phrases: HashSet<String>, strip: String, to: i64 },
}

struct CompiledScoreSpec {
    short: Vec<ShortStage>,
    post: Vec<PostStage>,
}

fn int_field(stage: &Value, key: &str) -> Result<i64, String> {
    field(stage, key)
        .and_then(JsonValueTrait::as_i64)
        .ok_or(format!("score stage missing int '{key}'"))
}

fn group_array<'a>(stage: &'a Value, key: &str) -> Result<&'a sonic_rs::Array, String> {
    field(stage, key)
        .and_then(JsonContainerTrait::as_array)
        .ok_or(format!("score stage missing array '{key}'"))
}

fn str_set(stage: &Value, key: &str) -> HashSet<String> {
    field(stage, key)
        .and_then(JsonContainerTrait::as_array)
        .into_iter()
        .flatten()
        .filter_map(JsonValueTrait::as_str)
        .map(String::from)
        .collect()
}

fn compile_spec(spec_json: &str) -> Result<CompiledScoreSpec, String> {
    let root: Value = sonic_rs::from_str(spec_json).map_err(|e| format!("invalid score spec json: {e}"))?;
    let stages = field(&root, "stages")
        .and_then(JsonContainerTrait::as_array)
        .ok_or("score spec missing 'stages' array")?;
    let mut short = Vec::new();
    let mut post = Vec::new();
    for stage in stages {
        match field_str(stage, "kind").ok_or("score stage missing 'kind'")? {
            "FrustrationShortCircuit" => short.push(ShortStage::Frustration {
                re: compile_group_array(group_array(stage, "groups")?, field_bool(stage, "ignore_case"))?,
                score: int_field(stage, "score")?,
            }),
            "PositiveClamp" => post.push(PostStage::PositiveClamp {
                from: int_field(stage, "from_score")?,
                to: int_field(stage, "to_score")?,
                max_words: int_field(stage, "max_words")? as usize,
                floor: int_field(stage, "positive_floor")? as i32,
            }),
            "MildIrritationDemote" => post.push(PostStage::MildIrritation {
                trigger: compile_group_array(group_array(stage, "trigger_groups")?, field_bool(stage, "ignore_case"))?,
                hostile: compile_group_array(group_array(stage, "hostile_groups")?, field_bool(stage, "ignore_case"))?,
                from: int_field(stage, "from_score")?,
                to: int_field(stage, "to_score")?,
                floor: int_field(stage, "hostile_floor")? as i32,
            }),
            "ResumeClamp" => post.push(PostStage::ResumeClamp {
                phrases: str_set(stage, "phrases"),
                strip: field_str(stage, "strip_trailing").unwrap_or("").to_string(),
                to: int_field(stage, "to_score")?,
            }),
            other => return Err(format!("unknown score stage kind: {other}")),
        }
    }
    Ok(CompiledScoreSpec { short, post })
}

pub fn score_short_circuit(spec_json: &str, buckets: &[Vec<String>]) -> Result<Vec<Option<i64>>, String> {
    let spec = compile_spec(spec_json)?;
    Ok(buckets.iter().map(|texts| short_circuit_one(&spec, texts)).collect())
}

fn short_circuit_one(spec: &CompiledScoreSpec, texts: &[String]) -> Option<i64> {
    spec.short.iter().find_map(|stage| match stage {
        ShortStage::Frustration { re, score } => texts.iter().any(|t| re.is_match(t)).then_some(*score),
    })
}

pub fn score_post_process(spec_json: &str, buckets: &[Vec<String>], raw: &[i64]) -> Result<Vec<i64>, String> {
    let spec = compile_spec(spec_json)?;
    Ok(buckets
        .iter()
        .zip(raw.iter())
        .map(|(texts, &score)| spec.post.iter().fold(score, |score, stage| apply_post(stage, texts, score)))
        .collect())
}

fn apply_post(stage: &PostStage, texts: &[String], score: i64) -> i64 {
    match stage {
        PostStage::PositiveClamp { from, to, max_words, floor } => {
            let clamp = score == *from
                && texts
                    .iter()
                    .any(|t| t.split_whitespace().count() <= *max_words && !lexicon::has_hit(t, *floor, false));
            if clamp {
                *to
            } else {
                score
            }
        }
        PostStage::MildIrritation { trigger, hostile, from, to, floor } => {
            let demote = score == *from
                && texts
                    .iter()
                    .any(|t| trigger.is_match(t) && !(hostile.is_match(t) || lexicon::has_hit(t, *floor, true)));
            if demote {
                *to
            } else {
                score
            }
        }
        PostStage::ResumeClamp { phrases, strip, to } => {
            if texts.iter().any(|t| phrases.contains(&normalize_bare(t, strip))) {
                *to
            } else {
                score
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SPEC: &str = r#"{"stages":[
        {"kind":"FrustrationShortCircuit","groups":[["frustration","\\bwtf\\b"]],"score":1,"ignore_case":true},
        {"kind":"ResumeClamp","phrases":["continue","go ahead"],"to_score":3,"strip_trailing":".!?,;:"}
    ]}"#;

    fn bucket(texts: &[&str]) -> Vec<String> {
        texts.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn frustration_short_circuits_first() {
        let buckets = [bucket(&["wtf is this"]), bucket(&["please continue carefully"])];
        assert_eq!(score_short_circuit(SPEC, &buckets).unwrap(), vec![Some(1), None]);
    }

    #[test]
    fn resume_clamps_in_post_process() {
        let buckets = [bucket(&["go ahead."]), bucket(&["a normal longer message here"])];
        assert_eq!(score_post_process(SPEC, &buckets, &[5, 5]).unwrap(), vec![3, 5]);
    }

    #[test]
    fn unknown_stage_is_an_error() {
        assert!(compile_spec(r#"{"stages":[{"kind":"Nope"}]}"#).is_err());
    }
}
