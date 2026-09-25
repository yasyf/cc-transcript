use std::collections::{HashMap, HashSet};
use std::mem::size_of;
use std::ops::Range;

use regex::{Regex, RegexBuilder};

use crate::activity::{result_index, tool_result_metadata};
use crate::filter::event_kind;
use crate::render::{haystack, haystack_bound, tool_haystack, tool_haystack_bound};
use crate::scan::ScanBudget;
use crate::snapshot::{Cancellation, SnapshotError, Status, TranscriptSnapshot};
use crate::toolcall::{expand_tool_names, with_registry, ToolRegistrySnapshot};
use crate::types::{matches_names, ContentBlock, Entry};

pub struct GrepPatternSpec {
    pub id: usize,
    pub pattern: String,
    pub max_matches: Option<usize>,
}

pub struct GrepOptions {
    pub kinds: Vec<String>,
    pub tool: Option<String>,
    pub errors: bool,
    pub where_text: bool,
    pub where_thinking: bool,
    pub where_tools: bool,
    pub context: usize,
    pub with_result: bool,
    pub ignore_case: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub struct GrepHit {
    pub event_index: usize,
    pub pattern_ids: Vec<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GrepResultMetadata<'a> {
    pub tool: &'a str,
    pub is_error: bool,
    pub denied: bool,
    pub duration_ms: Option<i64>,
}

pub struct GrepSourceResult<'a> {
    pub hits: Vec<GrepHit>,
    pub windows: Vec<Range<usize>>,
    pub names: HashMap<&'a str, &'a str>,
    pub results: HashMap<&'a str, GrepResultMetadata<'a>>,
    pub quota_reached: bool,
    pub source_complete: bool,
}

struct CompiledPattern {
    id: usize,
    regex: Regex,
    max_matches: Option<usize>,
}

pub struct GrepReducer {
    patterns: Vec<CompiledPattern>,
    counts: Vec<usize>,
    options: GrepOptions,
    tool_names: Option<HashSet<String>>,
    registry: std::sync::Arc<ToolRegistrySnapshot>,
    matched_items: usize,
    haystacks_built: usize,
    sources_indexed: usize,
    coverage_complete: bool,
}

fn incomplete(reason: &str) -> SnapshotError {
    SnapshotError::new(Status::Incomplete, reason)
}

fn invalid(reason: &str) -> SnapshotError {
    SnapshotError::new(Status::InvalidRequest, reason)
}

fn reserve_next<T>(
    items: &mut Vec<T>,
    budget: &mut ScanBudget,
    cancel: &Cancellation,
) -> Result<(), SnapshotError> {
    if items.len() == items.capacity() {
        let next = items.capacity().max(4).saturating_mul(2);
        budget.charge_projection(
            next.saturating_mul(size_of::<T>()).saturating_mul(2),
            0,
            cancel,
        )?;
        items.reserve_exact(next - items.len());
    }
    Ok(())
}

fn entry_bytes(snapshot: &TranscriptSnapshot, index: usize) -> usize {
    let at = snapshot
        .chunks
        .partition_point(|chunk| chunk.start <= index)
        - 1;
    let chunk = &snapshot.chunks[at];
    let charge = chunk.entry_charges[index - chunk.start];
    charge
        .owned_capacity_bytes
        .saturating_add(charge.opaque_dom_accounted_bytes)
}

impl GrepReducer {
    pub fn new(
        patterns: Vec<GrepPatternSpec>,
        options: GrepOptions,
        budget: &mut ScanBudget,
        cancel: &Cancellation,
    ) -> Result<Self, SnapshotError> {
        budget.checkpoint(cancel)?;
        if patterns.is_empty() || patterns.len() > 64 {
            return Err(invalid("grep requires between 1 and 64 patterns"));
        }
        let bytes = patterns
            .iter()
            .try_fold(0usize, |total, pattern| {
                total.checked_add(pattern.pattern.len())
            })
            .ok_or_else(|| invalid("pattern byte count overflow"))?;
        if bytes > 65_536 {
            return Err(invalid("grep pattern bytes exceed 65536"));
        }
        for (index, pattern) in patterns.iter().enumerate() {
            if patterns[..index].iter().any(|other| other.id == pattern.id) {
                return Err(invalid("grep pattern ids must be unique"));
            }
        }
        let registry = ToolRegistrySnapshot::capture_current();
        let option_bytes = options
            .kinds
            .iter()
            .map(String::len)
            .sum::<usize>()
            .saturating_add(options.tool.as_ref().map_or(0, String::len));
        budget.charge_projection(
            bytes
                .saturating_add(option_bytes)
                .saturating_mul(32)
                .saturating_add(patterns.len().saturating_mul(256)),
            0,
            cancel,
        )?;
        let compiled_budget = (budget.remaining().max_read_bytes / 4).min(1024 * 1024);
        let per_pattern = compiled_budget / patterns.len() / 4;
        if per_pattern < 256 {
            return Err(incomplete("insufficient regex compilation budget"));
        }
        budget.charge_projection(compiled_budget, 0, cancel)?;
        let mut compiled = Vec::with_capacity(patterns.len());
        for pattern in patterns {
            budget.checkpoint(cancel)?;
            let regex = RegexBuilder::new(&pattern.pattern)
                .case_insensitive(options.ignore_case)
                .size_limit(per_pattern)
                .dfa_size_limit(per_pattern)
                .nest_limit(64)
                .build()
                .map_err(|error| match error {
                    regex::Error::CompiledTooBig(_) => {
                        incomplete("regex compiled size limit exceeded")
                    }
                    _ => SnapshotError::new(Status::InvalidRequest, error.to_string()),
                })?;
            compiled.push(CompiledPattern {
                id: pattern.id,
                regex,
                max_matches: pattern.max_matches.filter(|cap| *cap != 0),
            });
        }
        let tool_names = if let Some(spec) = &options.tool {
            let registry_bytes = registry
                .accounted_allocations()
                .iter()
                .map(|(_, bytes)| *bytes)
                .sum::<usize>();
            budget.charge_projection(
                registry_bytes
                    .saturating_add(spec.len())
                    .saturating_mul(8)
                    .saturating_add(8192),
                0,
                cancel,
            )?;
            Some(with_registry(registry.clone(), || expand_tool_names(spec)))
        } else {
            None
        };
        Ok(Self {
            counts: vec![0; compiled.len()],
            patterns: compiled,
            options,
            tool_names,
            registry,
            matched_items: 0,
            haystacks_built: 0,
            sources_indexed: 0,
            coverage_complete: true,
        })
    }

    pub fn counts(&self) -> &[usize] {
        &self.counts
    }

    pub fn satisfied(&self) -> bool {
        self.patterns
            .iter()
            .zip(&self.counts)
            .all(|(pattern, count)| pattern.max_matches.is_some_and(|cap| *count >= cap))
    }

    pub fn complete(&self) -> bool {
        self.coverage_complete
    }

    fn mark_skipped_patterns(&mut self) {
        if self
            .patterns
            .iter()
            .zip(&self.counts)
            .any(|(pattern, count)| pattern.max_matches.is_some_and(|cap| *count >= cap))
        {
            self.coverage_complete = false;
        }
    }

    pub fn scan_text(
        &mut self,
        text: &str,
        budget: &mut ScanBudget,
        cancel: &Cancellation,
    ) -> Result<Vec<usize>, SnapshotError> {
        budget.charge_projection(
            text.len().saturating_add(
                self.patterns
                    .len()
                    .saturating_mul(size_of::<bool>() + size_of::<usize>()),
            ),
            1,
            cancel,
        )?;
        self.mark_skipped_patterns();
        let mut matched = vec![false; self.patterns.len()];
        self.match_text(text, &mut matched, budget, cancel)?;
        self.commit_matches(&matched, budget, cancel)
    }

    fn commit_matches(
        &mut self,
        matched: &[bool],
        budget: &mut ScanBudget,
        cancel: &Cancellation,
    ) -> Result<Vec<usize>, SnapshotError> {
        let count = matched.iter().filter(|matched| **matched).count();
        if self.matched_items.saturating_add(count) > budget.limits.max_items {
            return Err(incomplete("matched pattern item budget exhausted"));
        }
        budget.charge_projection(count.saturating_mul(size_of::<usize>()), 0, cancel)?;
        let mut ids = Vec::with_capacity(count);
        for (index, did_match) in matched.iter().enumerate() {
            if *did_match {
                self.counts[index] += 1;
                ids.push(self.patterns[index].id);
            }
        }
        self.matched_items += count;
        Ok(ids)
    }

    fn tool_matches(&self, name: &str) -> bool {
        self.tool_names
            .as_ref()
            .is_none_or(|names| with_registry(self.registry.clone(), || matches_names(name, names)))
    }

    fn uses_tool(&self, event: &Entry, names: &HashMap<&str, &str>) -> bool {
        match event {
            Entry::Assistant(_) => event.tool_uses().any(|tool| self.tool_matches(&tool.name)),
            Entry::User(_) => event.tool_results().any(|result| {
                names
                    .get(result.tool_use_id.as_str())
                    .is_some_and(|name| self.tool_matches(name))
            }),
            _ => false,
        }
    }

    pub fn scan_source<'a>(
        &mut self,
        snapshot: &'a TranscriptSnapshot,
        budget: &mut ScanBudget,
        cancel: &Cancellation,
    ) -> Result<GrepSourceResult<'a>, SnapshotError> {
        budget.checkpoint(cancel)?;
        if self.satisfied() {
            if snapshot.event_count != 0 {
                self.coverage_complete = false;
            }
            return Ok(GrepSourceResult {
                hits: Vec::new(),
                windows: Vec::new(),
                names: HashMap::new(),
                results: HashMap::new(),
                quota_reached: true,
                source_complete: snapshot.event_count == 0,
            });
        }
        let mut index_bytes = snapshot.event_count.saturating_mul(size_of::<&Entry>());
        for index in 0..snapshot.event_count {
            budget.charge_projection(0, 1, cancel)?;
            index_bytes = index_bytes
                .saturating_add(snapshot.entry(index).blocks().len().saturating_mul(512));
            if index_bytes > budget.remaining().max_read_bytes {
                return Err(incomplete("tool index exceeds projection budget"));
            }
        }
        budget.charge_projection(index_bytes, 0, cancel)?;
        let entries = snapshot.entries();
        let joined = result_index(&entries);
        budget.checkpoint(cancel)?;
        let joined: HashMap<_, _> = joined
            .iter()
            .map(|result| (result.tool_use_id, result))
            .collect();
        let mut names = HashMap::new();
        let mut results = HashMap::new();
        for entry in &entries {
            budget.checkpoint(cancel)?;
            for tool in entry.tool_uses() {
                names.insert(tool.id.as_str(), tool.name.as_str());
            }
            if let Entry::Assistant(assistant) = entry {
                for tool in entry.tool_uses() {
                    let result = joined.get(tool.id.as_str());
                    let metadata = tool_result_metadata(
                        assistant.meta.timestamp,
                        result.map(|result| result.block),
                        result.and_then(|result| result.result_ts),
                    );
                    results.insert(
                        tool.id.as_str(),
                        GrepResultMetadata {
                            tool: tool.name.as_str(),
                            is_error: metadata.is_error,
                            denied: metadata.denied,
                            duration_ms: metadata.duration_ms,
                        },
                    );
                }
            }
        }
        self.sources_indexed += 1;
        let mut output = GrepSourceResult {
            hits: Vec::new(),
            windows: Vec::new(),
            names,
            results,
            quota_reached: false,
            source_complete: true,
        };
        for index in 0..snapshot.event_count {
            budget.charge_projection(0, 1, cancel)?;
            self.mark_skipped_patterns();
            let event = snapshot.entry(index);
            if !self.options.kinds.is_empty()
                && !self
                    .options
                    .kinds
                    .iter()
                    .any(|kind| kind == event_kind(event))
            {
                continue;
            }
            if !self.options.errors
                && self.tool_names.is_some()
                && !self.uses_tool(event, &output.names)
            {
                continue;
            }
            if self.options.errors && !self.options.where_tools {
                continue;
            }
            budget.charge_projection(entry_bytes(snapshot, index).saturating_mul(4), 0, cancel)?;
            budget.charge_projection(
                self.patterns
                    .len()
                    .saturating_mul(size_of::<bool>() + size_of::<usize>()),
                0,
                cancel,
            )?;
            let mut matched = vec![false; self.patterns.len()];
            if self.options.errors {
                for block in event.blocks() {
                    budget.checkpoint(cancel)?;
                    let id = match block {
                        ContentBlock::ToolUse(tool) => tool.id.as_str(),
                        ContentBlock::ToolResult(result) => result.tool_use_id.as_str(),
                        _ => continue,
                    };
                    if !output
                        .results
                        .get(id)
                        .is_some_and(|result| result.is_error && self.tool_matches(result.tool))
                    {
                        continue;
                    }
                    let bytes = tool_haystack_bound(block, budget.remaining().max_read_bytes / 3)?;
                    budget.charge_projection(bytes.saturating_mul(3), 0, cancel)?;
                    let text = tool_haystack(block);
                    self.haystacks_built += 1;
                    self.match_text(&text, &mut matched, budget, cancel)?;
                }
            } else {
                let bytes = haystack_bound(
                    event,
                    self.options.where_text,
                    self.options.where_thinking,
                    self.options.where_tools,
                    budget.remaining().max_read_bytes / 3,
                )?;
                budget.charge_projection(bytes.saturating_mul(3), 0, cancel)?;
                let text = haystack(
                    event,
                    self.options.where_text,
                    self.options.where_thinking,
                    self.options.where_tools,
                );
                self.haystacks_built += 1;
                self.match_text(&text, &mut matched, budget, cancel)?;
            }
            if !matched.iter().any(|matched| *matched) {
                continue;
            }
            reserve_next(&mut output.hits, budget, cancel)?;
            reserve_next(&mut output.windows, budget, cancel)?;
            let pattern_ids = self.commit_matches(&matched, budget, cancel)?;
            output.hits.push(GrepHit {
                event_index: index,
                pattern_ids,
            });
            let range = index.saturating_sub(self.options.context)
                ..index
                    .saturating_add(self.options.context)
                    .saturating_add(1)
                    .min(snapshot.event_count);
            if let Some(last) = output
                .windows
                .last_mut()
                .filter(|last| range.start <= last.end)
            {
                last.end = last.end.max(range.end);
            } else {
                output.windows.push(range);
            }
            if self.satisfied() {
                output.quota_reached = true;
                output.source_complete =
                    index + 1 == snapshot.event_count && self.coverage_complete;
                break;
            }
        }
        output.source_complete &= self.coverage_complete;
        Ok(output)
    }

    fn match_text(
        &self,
        text: &str,
        matched: &mut [bool],
        budget: &ScanBudget,
        cancel: &Cancellation,
    ) -> Result<(), SnapshotError> {
        for (index, pattern) in self.patterns.iter().enumerate() {
            budget.checkpoint(cancel)?;
            if matched[index]
                || pattern
                    .max_matches
                    .is_some_and(|cap| self.counts[index] >= cap)
            {
                continue;
            }
            matched[index] = pattern.regex.is_match(text);
        }
        Ok(())
    }
}

impl GrepSourceResult<'_> {
    pub fn preflight_render(
        &self,
        snapshot: &TranscriptSnapshot,
        index: usize,
        budget: &mut ScanBudget,
        cancel: &Cancellation,
    ) -> Result<(), SnapshotError> {
        budget.checkpoint(cancel)?;
        if index >= snapshot.event_count {
            return Err(invalid("render event index is outside snapshot"));
        }
        let mut bytes = entry_bytes(snapshot, index)
            .saturating_mul(16)
            .saturating_add(
                snapshot
                    .canonical_path
                    .as_os_str()
                    .len()
                    .saturating_add(snapshot.id.len())
                    .saturating_mul(8),
            )
            .saturating_add(4096);
        for result in snapshot.entry(index).tool_results() {
            bytes = bytes.saturating_add(
                self.names
                    .get(result.tool_use_id.as_str())
                    .map_or(0, |name| name.len())
                    .saturating_mul(8),
            );
        }
        if bytes > budget.remaining().max_output_bytes {
            return Err(SnapshotError::new(
                Status::OutputLimit,
                "event render exceeds remaining output budget",
            ));
        }
        budget.charge_projection(bytes, 1, cancel)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::Provider;
    use crate::parse::parse_entry;
    use crate::snapshot::{EntryChunk, SourceIdentity, SourceStamp, WorkLimits};
    use crate::snapshot_activity::ActivityIndex;
    use sonic_rs::{json, Value};
    use std::path::PathBuf;
    use std::sync::Arc;

    fn snapshot(raw: &[Value]) -> TranscriptSnapshot {
        let entries: Vec<_> = raw
            .iter()
            .map(|value| parse_entry(value.clone()).unwrap())
            .collect();
        let activity = ActivityIndex::new(&entries.iter().collect::<Vec<_>>(), None);
        let event_count = entries.len();
        TranscriptSnapshot {
            id: "generation".into(),
            canonical_path: PathBuf::from("/test.jsonl"),
            stamp: SourceStamp {
                identity: SourceIdentity {
                    device: 1,
                    inode: 1,
                },
                size: 0,
                mtime_ns: 0,
                ctime_ns: 0,
            },
            provider: Provider::Claude,
            session_id: "s".into(),
            chunks: vec![Arc::new(EntryChunk::new(0, entries))],
            activity: Arc::new(activity),
            committed_bytes: 0,
            provisional_tail: false,
            fence: Vec::new(),
            event_count,
        }
    }

    fn user(text: &str) -> Value {
        json!({"type":"user","uuid":"u","sessionId":"s","timestamp":"2026-01-01T00:00:00Z","message":{"content":text}})
    }

    fn assistant(blocks: Value) -> Value {
        json!({"type":"assistant","uuid":"a","sessionId":"s","timestamp":"2026-01-01T00:00:00Z","message":{"model":"test","content":blocks}})
    }

    fn tool(id: &str, name: &str, input: Value) -> Value {
        json!({"type":"tool_use","id":id,"name":name,"input":input})
    }

    fn result(id: &str, text: &str, error: bool) -> Value {
        json!({"type":"user","uuid":"r","sessionId":"s","timestamp":"2026-01-01T00:00:00.010500Z","message":{"content":[{"type":"tool_result","tool_use_id":id,"content":text,"is_error":error}]}})
    }

    fn options() -> GrepOptions {
        GrepOptions {
            kinds: vec![],
            tool: None,
            errors: false,
            where_text: true,
            where_thinking: true,
            where_tools: true,
            context: 0,
            with_result: false,
            ignore_case: false,
        }
    }

    fn budget() -> ScanBudget {
        ScanBudget::new(WorkLimits {
            max_read_bytes: 8 * 1024 * 1024,
            max_events: 4096,
            max_items: 4096,
            max_output_bytes: 1024 * 1024,
            max_sources: 100,
            max_discovery_entries: 100,
            deadline_unix_ms: u64::MAX,
        })
    }

    fn reducer(
        patterns: &[(&str, Option<usize>)],
        options: GrepOptions,
        budget: &mut ScanBudget,
    ) -> GrepReducer {
        GrepReducer::new(
            patterns
                .iter()
                .enumerate()
                .map(|(id, (pattern, cap))| GrepPatternSpec {
                    id,
                    pattern: (*pattern).into(),
                    max_matches: *cap,
                })
                .collect(),
            options,
            budget,
            &Cancellation::default(),
        )
        .unwrap()
    }

    #[test]
    fn eighteen_patterns_share_one_index_and_one_haystack_per_event() {
        let source = snapshot(&[user("needle"), user("other")]);
        let mut budget = budget();
        let mut grep = reducer(&vec![("needle", None); 18], options(), &mut budget);
        let result = grep
            .scan_source(&source, &mut budget, &Cancellation::default())
            .unwrap();
        assert_eq!(
            result.hits,
            vec![GrepHit {
                event_index: 0,
                pattern_ids: (0..18).collect()
            }]
        );
        assert_eq!(grep.counts(), &[1; 18]);
        assert_eq!(grep.haystacks_built, 2);
        assert_eq!(grep.sources_indexed, 1);
        assert!(result.source_complete);
    }

    #[test]
    fn overlapping_patterns_have_independent_global_quotas() {
        let source = snapshot(&[user("ab"), user("a"), user("b"), user("tail")]);
        let mut budget = budget();
        let mut grep = reducer(&[("a", Some(1)), ("b", Some(2))], options(), &mut budget);
        let result = grep
            .scan_source(&source, &mut budget, &Cancellation::default())
            .unwrap();
        assert_eq!(
            result.hits,
            vec![
                GrepHit {
                    event_index: 0,
                    pattern_ids: vec![0, 1]
                },
                GrepHit {
                    event_index: 2,
                    pattern_ids: vec![1]
                }
            ]
        );
        assert_eq!(grep.counts(), &[1, 2]);
        assert!(result.quota_reached);
        assert!(!result.source_complete);
        assert_eq!(grep.haystacks_built, 3);
        let result = grep
            .scan_source(&source, &mut budget, &Cancellation::default())
            .unwrap();
        assert!(result.hits.is_empty());
        assert_eq!(grep.sources_indexed, 1);
    }

    #[test]
    fn exact_final_event_quota_is_complete_and_zero_is_unlimited() {
        let source = snapshot(&[user("hit"), user("hit")]);
        let mut budget = budget();
        let mut grep = reducer(&[("hit", Some(2))], options(), &mut budget);
        let result = grep
            .scan_source(&source, &mut budget, &Cancellation::default())
            .unwrap();
        assert!(result.quota_reached && result.source_complete);
        let mut grep = reducer(&[("hit", Some(0))], options(), &mut budget);
        let result = grep
            .scan_source(&source, &mut budget, &Cancellation::default())
            .unwrap();
        assert!(!result.quota_reached && result.source_complete);
        assert_eq!(grep.counts(), &[2]);
    }

    #[test]
    fn later_results_keep_error_denial_and_duration_semantics() {
        let source = snapshot(&[
            assistant(json!([tool("t", "Bash", json!({"command":"oops"}))])),
            result("t", "oops", true),
        ]);
        let mut budget = budget();
        let mut opts = options();
        opts.errors = true;
        opts.tool = Some("Bash".into());
        let mut grep = reducer(&[("oops", Some(1))], opts, &mut budget);
        let result = grep
            .scan_source(&source, &mut budget, &Cancellation::default())
            .unwrap();
        assert_eq!(result.hits[0].event_index, 0);
        assert_eq!(
            result.results["t"],
            GrepResultMetadata {
                tool: "Bash",
                is_error: true,
                denied: false,
                duration_ms: Some(10)
            }
        );
        assert!(!result.source_complete);
    }

    #[test]
    fn failed_blocks_match_individually_and_count_events_once() {
        let source = snapshot(&[
            assistant(json!([
                tool("a", "Bash", json!({"command":"first"})),
                tool("b", "Bash", json!({"command":"second"}))
            ])),
            result("a", "failed", true),
            result("b", "failed", true),
        ]);
        let mut budget = budget();
        let mut opts = options();
        opts.errors = true;
        opts.kinds = vec!["assistant".into()];
        let mut grep = reducer(
            &[("Bash", None), ("(?s)first.*second", None)],
            opts,
            &mut budget,
        );
        let result = grep
            .scan_source(&source, &mut budget, &Cancellation::default())
            .unwrap();
        assert_eq!(
            result.hits,
            vec![GrepHit {
                event_index: 0,
                pattern_ids: vec![0]
            }]
        );
        assert_eq!(grep.counts(), &[1, 0]);
        assert_eq!(grep.haystacks_built, 2);
    }

    #[test]
    fn tool_filter_resolves_later_names_and_last_duplicate_result() {
        let source = snapshot(&[
            result("t", "found", true),
            assistant(json!([tool("t", "Bash", json!({}))])),
            result("t", "success", false),
        ]);
        let mut budget = budget();
        let mut opts = options();
        opts.tool = Some("Bash".into());
        let mut grep = reducer(&[("found", None)], opts, &mut budget);
        let result = grep
            .scan_source(&source, &mut budget, &Cancellation::default())
            .unwrap();
        assert_eq!(result.hits[0].event_index, 0);
        assert!(!result.results["t"].is_error);
        assert_eq!(result.names["t"], "Bash");
    }

    #[test]
    fn context_windows_merge_and_case_flags_hold() {
        let source = snapshot(&[
            user("before"),
            user("HIT"),
            user("middle"),
            user("hit"),
            user("after"),
            user("tail"),
        ]);
        let mut budget = budget();
        let mut opts = options();
        opts.context = 1;
        opts.ignore_case = true;
        let mut grep = reducer(&[("hit", None)], opts, &mut budget);
        let result = grep
            .scan_source(&source, &mut budget, &Cancellation::default())
            .unwrap();
        assert_eq!(result.windows, vec![0..5]);
        assert_eq!(grep.counts(), &[2]);
    }

    #[test]
    fn oversized_event_is_rejected_before_haystack_production() {
        let source = snapshot(&[user(&"x".repeat(32768)), user("never")]);
        let mut budget = budget();
        let mut grep = reducer(&[("x", None)], options(), &mut budget);
        budget.limits.max_read_bytes = budget.progress.projection_bytes + 8192;
        let error = grep
            .scan_source(&source, &mut budget, &Cancellation::default())
            .err()
            .unwrap();
        assert_eq!(error.status, Status::Incomplete);
        assert_eq!(grep.haystacks_built, 0);
        assert_eq!(grep.counts(), &[0]);
    }

    #[test]
    fn context_render_rejects_output_before_producer_is_visited() {
        let source = snapshot(&[user("hit"), user(&"x".repeat(32768))]);
        let mut budget = budget();
        let mut opts = options();
        opts.context = 1;
        let mut grep = reducer(&[("hit", Some(1))], opts, &mut budget);
        let result = grep
            .scan_source(&source, &mut budget, &Cancellation::default())
            .unwrap();
        budget.limits.max_output_bytes = 1024;
        let mut visited = false;
        let admission = result
            .preflight_render(&source, 1, &mut budget, &Cancellation::default())
            .map(|()| {
                visited = true;
                haystack(source.entry(1), true, true, true)
            });
        assert_eq!(admission.err().unwrap().status, Status::OutputLimit);
        assert!(!visited);
    }

    #[test]
    fn cancelled_and_expired_scans_do_not_visit_source_or_producer() {
        let source = snapshot(&[user("hit")]);
        let mut budget = budget();
        let mut grep = reducer(&[("hit", None)], options(), &mut budget);
        let cancel = Cancellation::default();
        cancel.cancel();
        assert_eq!(
            grep.scan_source(&source, &mut budget, &cancel)
                .err()
                .unwrap()
                .status,
            Status::Cancelled
        );
        budget.limits.deadline_unix_ms = 0;
        assert_eq!(
            grep.scan_source(&source, &mut budget, &Cancellation::default())
                .err()
                .unwrap()
                .status,
            Status::Deadline
        );
        assert_eq!((grep.sources_indexed, grep.haystacks_built), (0, 0));
    }

    #[test]
    fn pattern_limits_and_duplicate_ids_fail_before_compile() {
        let mut budget = budget();
        let patterns = vec![
            GrepPatternSpec {
                id: 7,
                pattern: "a".into(),
                max_matches: None,
            },
            GrepPatternSpec {
                id: 7,
                pattern: "b".into(),
                max_matches: None,
            },
        ];
        assert_eq!(
            GrepReducer::new(patterns, options(), &mut budget, &Cancellation::default())
                .err()
                .unwrap()
                .status,
            Status::InvalidRequest
        );
        assert_eq!(budget.progress.projection_bytes, 0);
        budget.limits.max_read_bytes = 16;
        assert_eq!(
            GrepReducer::new(
                vec![GrepPatternSpec {
                    id: 0,
                    pattern: "a".into(),
                    max_matches: None
                }],
                options(),
                &mut budget,
                &Cancellation::default()
            )
            .err()
            .unwrap()
            .status,
            Status::Incomplete
        );
    }

    #[test]
    fn bounded_haystack_sizes_match_existing_renderers_for_all_masks() {
        let source = snapshot(&[
            user("a\n\t\"é"),
            assistant(
                json!([{"type":"text","text":""},{"type":"text","text":"a"},{"type":"thinking","thinking":"thought"},tool("t","Bash",json!({"command":"echo \"é\"","nested":[true,null,1.25,-0.0]}))]),
            ),
            result("t", "result", true),
        ]);
        for index in 0..source.event_count {
            for mask in 0..8 {
                let flags = (mask & 1 != 0, mask & 2 != 0, mask & 4 != 0);
                let actual = haystack(source.entry(index), flags.0, flags.1, flags.2);
                let bound =
                    haystack_bound(source.entry(index), flags.0, flags.1, flags.2, usize::MAX)
                        .unwrap();
                assert_eq!(bound, actual.len(), "event {index}, mask {mask}");
                if bound > 0 {
                    assert!(haystack_bound(
                        source.entry(index),
                        flags.0,
                        flags.1,
                        flags.2,
                        bound - 1
                    )
                    .is_err());
                }
            }
        }
    }
}
