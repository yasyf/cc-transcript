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

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub enum FactsNeed {
    Skills,
    Index,
    Full,
}

impl FactsNeed {
    pub fn from_raw(raw: u64) -> Option<Self> {
        match raw {
            0 => Some(Self::Skills),
            1 => Some(Self::Index),
            2 => Some(Self::Full),
            _ => None,
        }
    }

    pub fn of_predicate(query: &Value) -> Self {
        match query.get("kind").and_then(Value::as_str) {
            Some("has_override") => Self::Full,
            Some("has_skill" | "has_skill_suffix")
                if query
                    .get("values")
                    .and_then(Value::as_array)
                    .is_some_and(|values| values.iter().all(|value| value.as_str().is_some())) =>
            {
                Self::Skills
            }
            _ => Self::Index,
        }
    }

    pub fn of_query(query: &Value, observers: Option<&Value>) -> Self {
        let primary = Self::of_predicate(query);
        let Some(observers) = observers else {
            return primary;
        };
        let Some(queries) = observers.as_array() else {
            return primary.max(Self::Index);
        };
        let demand = queries
            .iter()
            .map(Self::of_predicate)
            .fold(primary, Self::max);
        if queries.len() > crate::snapshot::OBSERVER_LIMIT {
            demand.max(Self::Index)
        } else {
            demand
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub enum TextState {
    Unbuilt = 0,
    Present = 1,
    Exceeded = 2,
}

impl TextState {
    pub fn from_raw(raw: u64) -> Option<Self> {
        match raw {
            0 => Some(Self::Unbuilt),
            1 => Some(Self::Present),
            2 => Some(Self::Exceeded),
            _ => None,
        }
    }
}

pub enum OverrideFacts {
    Unloaded,
    Exceeded,
    Loaded(Vec<OverrideEvent>),
}

impl OverrideFacts {
    pub fn text_state(&self) -> TextState {
        match self {
            Self::Unloaded => TextState::Unbuilt,
            Self::Exceeded => TextState::Exceeded,
            Self::Loaded(_) => TextState::Present,
        }
    }

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

#[derive(serde::Serialize, serde::Deserialize)]
pub struct CodexSkillProof {
    pub covered_bytes: u64,
    pub line_bound: usize,
    pub tail_bytes: usize,
    pub first_line_committed: bool,
    pub fence_seal: [u8; 32],
}

impl CodexSkillProof {
    pub fn validates(&self, source_bytes: u64, entry_bound: usize) -> bool {
        self.covered_bytes == source_bytes
            && source_bytes != 0
            && self.line_bound != 0
            && self.line_bound <= entry_bound
            && self.line_bound as u64 <= source_bytes
            && self.tail_bytes <= self.line_bound
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillEvidence {
    Parsed { skills: Vec<String> },
    CodexFramed { proof: Box<CodexSkillProof> },
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PreparedEvidence {
    Index { inputs: Value, has_error: bool },
    Skills { evidence: SkillEvidence },
}

pub type DecodedIndexSection = PreparedEvidence;

pub struct PreparedFacts {
    pub evidence: PreparedEvidence,
    pub overrides: OverrideFacts,
    pub accounted: usize,
}

impl PreparedFacts {
    pub fn accounted_bytes(&self) -> usize {
        self.accounted
    }

    pub fn index_accounted_bytes(&self) -> usize {
        crate::snapshot_ledger::arc_bytes::<Self>()
            + match &self.evidence {
                PreparedEvidence::Index { inputs, .. } => {
                    let charge = crate::snapshot_memory::value_charge(inputs);
                    charge.owned_capacity_bytes + charge.opaque_dom_accounted_bytes
                }
                PreparedEvidence::Skills {
                    evidence: SkillEvidence::Parsed { skills },
                } => {
                    skills.capacity() * std::mem::size_of::<String>()
                        + skills.iter().map(String::capacity).sum::<usize>()
                }
                PreparedEvidence::Skills {
                    evidence: SkillEvidence::CodexFramed { .. },
                } => std::mem::size_of::<CodexSkillProof>(),
            }
    }

    pub fn refresh_accounted(&mut self) {
        self.accounted = self.index_accounted_bytes() + self.overrides.charge();
    }

    pub fn is_full(&self) -> bool {
        matches!(self.evidence, PreparedEvidence::Index { .. }) && self.overrides.is_loaded()
    }

    pub fn text_state(&self) -> TextState {
        self.overrides.text_state()
    }

    pub fn validates_source(&self, bytes: u64, entry_bound: usize) -> bool {
        match &self.evidence {
            PreparedEvidence::Skills {
                evidence: SkillEvidence::CodexFramed { proof },
            } => proof.validates(bytes, entry_bound),
            PreparedEvidence::Index { .. }
            | PreparedEvidence::Skills {
                evidence: SkillEvidence::Parsed { .. },
            } => true,
        }
    }

    pub fn capability(&self) -> FactsNeed {
        match &self.evidence {
            PreparedEvidence::Skills { .. } => FactsNeed::Skills,
            PreparedEvidence::Index { .. } if self.is_full() => FactsNeed::Full,
            PreparedEvidence::Index { .. } => FactsNeed::Index,
        }
    }

    pub fn serves(&self, need: FactsNeed) -> bool {
        match need {
            FactsNeed::Skills => true,
            FactsNeed::Index => matches!(self.evidence, PreparedEvidence::Index { .. }),
            FactsNeed::Full => self.is_full(),
        }
    }

    pub fn without_text(mut self) -> Self {
        self.overrides = OverrideFacts::Unloaded;
        self.refresh_accounted();
        self
    }

    pub fn index_section(&self) -> &PreparedEvidence {
        &self.evidence
    }

    pub fn index_inputs(&self) -> &Value {
        match &self.evidence {
            PreparedEvidence::Index { inputs, .. } => inputs,
            PreparedEvidence::Skills { .. } => unreachable!("Index capability is required"),
        }
    }

    pub fn has_error(&self) -> bool {
        match &self.evidence {
            PreparedEvidence::Index { has_error, .. } => *has_error,
            PreparedEvidence::Skills { .. } => unreachable!("Index capability is required"),
        }
    }

    pub fn index(inputs: Value, has_error: bool, overrides: OverrideFacts) -> Self {
        let mut facts = Self {
            evidence: PreparedEvidence::Index { inputs, has_error },
            overrides,
            accounted: 0,
        };
        facts.refresh_accounted();
        facts
    }

    pub fn skills(evidence: SkillEvidence) -> Self {
        let mut facts = Self {
            evidence: PreparedEvidence::Skills { evidence },
            overrides: OverrideFacts::Unloaded,
            accounted: 0,
        };
        facts.refresh_accounted();
        facts
    }

    pub fn text_section(&self) -> Option<&[OverrideEvent]> {
        self.overrides.section()
    }

    pub fn from_sections(index: DecodedIndexSection, overrides: OverrideFacts) -> Self {
        let mut facts = Self {
            evidence: index,
            overrides,
            accounted: 0,
        };
        facts.refresh_accounted();
        facts
    }

    pub fn query(&self, query: &Value) -> Result<Value, SnapshotError> {
        let kind = string(query, "kind")?;
        if matches!(kind, "has_skill" | "has_skill_suffix") {
            let names = strings(query, "values")?;
            let matches = |skill: &str| {
                names.contains(&skill)
                    || kind == "has_skill_suffix"
                        && names.contains(&skill.split_once(':').map_or(skill, |(_, tail)| tail))
            };
            let answer = match &self.evidence {
                PreparedEvidence::Index { inputs, .. } => inputs["skills"]
                    .as_array()
                    .ok_or_else(|| invalid("prepared skills missing"))?
                    .iter()
                    .filter_map(Value::as_str)
                    .any(matches),
                PreparedEvidence::Skills {
                    evidence: SkillEvidence::Parsed { skills },
                } => skills.iter().any(|skill| matches(skill)),
                PreparedEvidence::Skills {
                    evidence: SkillEvidence::CodexFramed { .. },
                } => false,
            };
            return Ok(json!({"kind":"scalar","value":answer}));
        }
        if !self.serves(FactsNeed::of_predicate(query)) {
            return Err(invalid("prepared evidence does not serve query"));
        }
        let inputs = self.index_inputs();
        let calls = inputs
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
                inputs["edited_files"]
                    .as_array()
                    .ok_or_else(|| invalid("prepared edits missing"))?
                    .iter()
                    .any(|file| {
                        file.get("path")
                            .and_then(Value::as_str)
                            .is_some_and(|path| FileRef::new(path).matches(&globs))
                    })
            }
            "has_command" => {
                #[cfg(feature = "command")]
                {
                    let argv = strings(query, "values")?;
                    inputs["commands"]
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
                inputs["commands"]
                    .as_array()
                    .ok_or_else(|| invalid("prepared commands missing"))?
                    .iter()
                    .filter_map(Value::as_str)
                    .any(|command| regex.is_match(command))
            }
            "has_error" => match self.evidence {
                PreparedEvidence::Index { has_error, .. } => has_error,
                PreparedEvidence::Skills { .. } => unreachable!("Index capability is required"),
            },
            "has_edit" => !inputs["edited_files"]
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typed_skills_demand_is_union_of_valid_primary_and_observers() {
        let query = json!({"kind":"has_skill","values":["x"]});
        let skills = json!([{"kind":"has_skill_suffix","values":["y"]}]);
        let tool = json!([{"kind":"has_tool","pattern":"Read"}]);
        let full = json!([{"kind":"has_override","token":"x","invalidated_by":[]}]);
        let malformed = json!([{"kind":"has_skill","values":[1]}]);
        assert_eq!(FactsNeed::of_query(&query, None), FactsNeed::Skills);
        assert_eq!(
            FactsNeed::of_query(&query, Some(&json!({"kind":"has_skill"}))),
            FactsNeed::Index
        );
        assert_eq!(
            FactsNeed::of_query(&query, Some(&json!([]))),
            FactsNeed::Skills
        );
        assert_eq!(
            FactsNeed::of_query(&query, Some(&skills)),
            FactsNeed::Skills
        );
        assert_eq!(FactsNeed::of_query(&query, Some(&tool)), FactsNeed::Index);
        assert_eq!(FactsNeed::of_query(&query, Some(&full)), FactsNeed::Full);
        assert_eq!(
            FactsNeed::of_query(&query, Some(&malformed)),
            FactsNeed::Index
        );
        assert_eq!(
            FactsNeed::of_predicate(&json!({"kind":"deep_predicate_inputs"})),
            FactsNeed::Index
        );
    }

    #[test]
    fn typed_skills_evidence_cannot_serve_index_or_full() {
        let facts = PreparedFacts::skills(SkillEvidence::Parsed {
            skills: vec!["plugin:x".to_owned()],
        });
        assert!(facts.serves(FactsNeed::Skills));
        assert!(!facts.serves(FactsNeed::Index));
        assert!(!facts.serves(FactsNeed::Full));
        assert_eq!(
            facts
                .query(&json!({"kind":"has_skill","values":["plugin:x"]}))
                .unwrap()["value"]
                .as_bool(),
            Some(true)
        );
        assert_eq!(
            facts
                .query(&json!({"kind":"has_skill_suffix","values":["x"]}))
                .unwrap()["value"]
                .as_bool(),
            Some(true)
        );
        assert!(facts
            .query(&json!({"kind":"has_tool","pattern":"Read"}))
            .is_err());
        let bytes = sonic_rs::to_vec(facts.index_section()).unwrap();
        let decoded: DecodedIndexSection = sonic_rs::from_slice(&bytes).unwrap();
        let restored = PreparedFacts::from_sections(decoded, OverrideFacts::Unloaded);
        assert!(restored.serves(FactsNeed::Skills));
        assert!(!restored.serves(FactsNeed::Index));
        assert!(!restored.serves(FactsNeed::Full));
    }
}
