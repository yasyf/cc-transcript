use regex::Regex;
use sonic_rs::{json, JsonContainerTrait, JsonValueTrait, Value};

#[cfg(feature = "command")]
use crate::command::CommandLine;
use crate::query::FileRef;
use crate::snapshot::{SnapshotError, Status};
use crate::toolcall::tool_name_matches;

fn invalid(reason: impl Into<String>) -> SnapshotError {
    SnapshotError::new(Status::InvalidRequest, reason)
}

fn incomplete(reason: impl Into<String>) -> SnapshotError {
    SnapshotError::new(Status::Incomplete, reason)
}

fn string<'a>(value: &'a Value, key: &str) -> Result<&'a str, SnapshotError> {
    value
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| invalid(format!("missing or invalid {key}")))
}

fn strings<'a>(value: &'a Value, key: &str) -> Result<Vec<&'a str>, SnapshotError> {
    value
        .get(key)
        .and_then(Value::as_array)
        .ok_or_else(|| invalid(format!("missing or invalid {key}")))?
        .iter()
        .map(|item| {
            item.as_str()
                .ok_or_else(|| invalid(format!("invalid {key}")))
        })
        .collect()
}

fn call_name(call: &Value) -> Option<&str> {
    call.as_array()
        .and_then(|items| items.first())
        .and_then(Value::as_str)
}

fn call_paths(call: &Value) -> Result<&sonic_rs::Array, SnapshotError> {
    call.as_array()
        .and_then(|items| items.get(1))
        .and_then(Value::as_array)
        .ok_or_else(|| invalid("prepared call paths missing"))
}

pub struct OverrideEvent {
    pub text: String,
    pub tools: Vec<String>,
}

pub struct PreparedFacts {
    pub inputs: Value,
    pub has_error: bool,
    pub override_events: Option<Vec<OverrideEvent>>,
    pub accounted: usize,
}

impl PreparedFacts {
    pub fn accounted_bytes(&self) -> usize {
        self.accounted
    }

    pub fn query(&self, query: &Value) -> Result<Value, SnapshotError> {
        let kind = string(query, "kind")?;
        let calls = self
            .inputs
            .get("calls")
            .and_then(Value::as_array)
            .ok_or_else(|| invalid("prepared calls missing"))?;
        let answer = match kind {
            "has_tool" => {
                let pattern = string(query, "pattern")?;
                calls.iter().any(|call| {
                    call_name(call).is_some_and(|tool| tool_name_matches(tool, pattern))
                })
            }
            "has_read" => {
                let pattern = string(query, "pattern")?;
                calls
                    .iter()
                    .filter(|call| {
                        call_name(call).is_some_and(|tool| tool_name_matches(tool, "Read"))
                    })
                    .try_fold(false, |found, call| {
                        Ok::<_, SnapshotError>(
                            found
                                || call_paths(call)?.iter().any(|path| {
                                    path.as_str().is_some_and(|path| path.contains(pattern))
                                }),
                        )
                    })?
            }
            "has_read_glob" => {
                let globs = strings(query, "values")?;
                calls
                    .iter()
                    .filter(|call| {
                        call_name(call).is_some_and(|tool| tool_name_matches(tool, "Read"))
                    })
                    .try_fold(false, |found, call| {
                        Ok::<_, SnapshotError>(
                            found
                                || call_paths(call)?.iter().any(|path| {
                                    path.as_str()
                                        .is_some_and(|path| FileRef::new(path).matches(&globs))
                                }),
                        )
                    })?
            }
            "has_edit_to" => {
                let globs = strings(query, "values")?;
                self.inputs["edited_files"]
                    .as_array()
                    .ok_or_else(|| invalid("prepared edits missing"))?
                    .iter()
                    .any(|file| {
                        file.get("path")
                            .and_then(Value::as_str)
                            .is_some_and(|path| FileRef::new(path).matches(&globs))
                    })
            }
            "has_skill" | "has_skill_suffix" => {
                let names = strings(query, "values")?;
                self.inputs["skills"]
                    .as_array()
                    .ok_or_else(|| invalid("prepared skills missing"))?
                    .iter()
                    .filter_map(Value::as_str)
                    .any(|skill| {
                        names.contains(&skill)
                            || kind == "has_skill_suffix"
                                && names.contains(
                                    &skill.split_once(':').map_or(skill, |(_, tail)| tail),
                                )
                    })
            }
            "has_command" => {
                #[cfg(feature = "command")]
                {
                    let argv = strings(query, "values")?;
                    self.inputs["commands"]
                        .as_array()
                        .ok_or_else(|| invalid("prepared commands missing"))?
                        .iter()
                        .filter_map(Value::as_str)
                        .any(|command| {
                            CommandLine::parse(command)
                                .commands()
                                .iter()
                                .any(|parsed| parsed.runs(&argv))
                        })
                }
                #[cfg(not(feature = "command"))]
                {
                    return Err(invalid("has_command requires the command feature"));
                }
            }
            "has_command_regex" => {
                let regex = Regex::new(string(query, "pattern")?)
                    .map_err(|error| invalid(error.to_string()))?;
                self.inputs["commands"]
                    .as_array()
                    .ok_or_else(|| invalid("prepared commands missing"))?
                    .iter()
                    .filter_map(Value::as_str)
                    .any(|command| regex.is_match(command))
            }
            "has_error" => self.has_error,
            "has_edit" => !self.inputs["edited_files"]
                .as_array()
                .ok_or_else(|| invalid("prepared edits missing"))?
                .is_empty(),
            "has_override" => {
                let events = self
                    .override_events
                    .as_ref()
                    .ok_or_else(|| incomplete("prepared override text exceeded its bound"))?;
                let token = string(query, "token")?;
                let invalidators = strings(query, "invalidated_by")?;
                let Some(last) = events.iter().rposition(|event| event.text.contains(token)) else {
                    return Ok(json!({"kind":"scalar","value":false}));
                };
                !events[last + 1..].iter().any(|event| {
                    event.tools.iter().any(|tool| {
                        invalidators
                            .iter()
                            .any(|name| tool_name_matches(tool, name))
                    })
                })
            }
            _ => return Err(invalid(format!("unsupported prepared query {kind}"))),
        };
        Ok(json!({"kind":"scalar","value":answer}))
    }
}
