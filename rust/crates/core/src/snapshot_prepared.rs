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

#[derive(serde::Serialize, serde::Deserialize)]
pub struct OverrideEvent {
    pub text: String,
    pub tools: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FactsNeed {
    Index,
    Full,
}

impl FactsNeed {
    pub fn of_query(query: &Value, observers: Option<&sonic_rs::Array>) -> Self {
        let wants_overrides =
            |query: &Value| query.get("kind").and_then(Value::as_str) == Some("has_override");
        if wants_overrides(query)
            || observers.is_some_and(|queries| queries.iter().any(wants_overrides))
        {
            Self::Full
        } else {
            Self::Index
        }
    }
}

pub enum OverrideFacts {
    Unloaded,
    Exceeded,
    Loaded(Vec<OverrideEvent>),
}

impl OverrideFacts {
    pub fn from_section(section: Option<Vec<OverrideEvent>>) -> Self {
        match section {
            None => Self::Exceeded,
            Some(events) => Self::Loaded(events),
        }
    }

    pub fn section(&self) -> Option<&[OverrideEvent]> {
        match self {
            Self::Unloaded => unreachable!("override text is written only from loaded facts"),
            Self::Exceeded => None,
            Self::Loaded(events) => Some(events),
        }
    }

    pub fn is_loaded(&self) -> bool {
        !matches!(self, Self::Unloaded)
    }

    fn charge(&self) -> usize {
        match self {
            Self::Unloaded | Self::Exceeded => 0,
            Self::Loaded(events) => {
                events.capacity() * std::mem::size_of::<OverrideEvent>()
                    + events
                        .iter()
                        .map(|event| {
                            event.text.capacity()
                                + event.tools.capacity() * std::mem::size_of::<String>()
                                + event.tools.iter().map(String::capacity).sum::<usize>()
                        })
                        .sum::<usize>()
            }
        }
    }
}

#[derive(serde::Serialize)]
pub struct IndexSection<'a> {
    pub inputs: &'a Value,
    pub has_error: bool,
}

#[derive(serde::Deserialize)]
pub struct DecodedIndexSection {
    pub inputs: Value,
    pub has_error: bool,
}

pub struct PreparedFacts {
    pub inputs: Value,
    pub has_error: bool,
    pub overrides: OverrideFacts,
    pub accounted: usize,
}

impl PreparedFacts {
    pub fn accounted_bytes(&self) -> usize {
        self.accounted
    }

    pub fn index_accounted_bytes(&self) -> usize {
        let input_charge = crate::snapshot_memory::value_charge(&self.inputs);
        crate::snapshot_ledger::arc_bytes::<Self>()
            + input_charge.owned_capacity_bytes
            + input_charge.opaque_dom_accounted_bytes
    }

    pub fn refresh_accounted(&mut self) {
        self.accounted = self.index_accounted_bytes() + self.overrides.charge();
    }

    pub fn is_full(&self) -> bool {
        self.overrides.is_loaded()
    }

    pub fn serves(&self, need: FactsNeed) -> bool {
        need == FactsNeed::Index || self.is_full()
    }

    pub fn without_text(mut self) -> Self {
        self.overrides = OverrideFacts::Unloaded;
        self.refresh_accounted();
        self
    }

    pub fn index_section(&self) -> IndexSection<'_> {
        IndexSection {
            inputs: &self.inputs,
            has_error: self.has_error,
        }
    }

    pub fn text_section(&self) -> Option<&[OverrideEvent]> {
        self.overrides.section()
    }

    pub fn from_sections(index: DecodedIndexSection, overrides: OverrideFacts) -> Self {
        let mut facts = Self {
            inputs: index.inputs,
            has_error: index.has_error,
            overrides,
            accounted: 0,
        };
        facts.refresh_accounted();
        facts
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
                let events = match &self.overrides {
                    OverrideFacts::Unloaded => {
                        unreachable!("has_override is answered only from fully loaded facts")
                    }
                    OverrideFacts::Exceeded => {
                        return Err(incomplete("prepared override text exceeded its bound"))
                    }
                    OverrideFacts::Loaded(events) => events,
                };
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
