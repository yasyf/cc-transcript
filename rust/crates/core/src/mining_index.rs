use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::mem::size_of;

use super::{
    structural_re, CompiledMiningSpec, MiningError, MiningResult, MiningSignal, MiningUsage,
};
use crate::literals::mining::{
    DETECTOR_ASK_USER_QUESTION, DETECTOR_DENIAL, DETECTOR_EXIT_PLAN_REJECTION, DETECTOR_INTERRUPT,
    DETECTOR_PLAN_REENTRY, DETECTOR_REVIEW_COMMENT, DETECTOR_TRANSCRIPT_MESSAGE,
};
use crate::protocol::{
    embedded_user_text, interrupt_marker, ANSWERED_PREFIX, ANSWERED_TRAILER,
    DENIAL_KIND_USER_REJECTED,
};
use crate::snapshot::{Cancellation, SnapshotError, Status, WorkLimits};
use crate::snapshot_memory::{entry_charge, tool_use_charge};
use crate::snapshot_review::{Followup, ReviewChanges, ReviewDependencies};
use crate::types::{
    matches_names, AssistantEntry, ContentBlock, Entry, ModeEntry, OtherEntry, ToolResultBlock,
    ToolUseBlock, UserContent, UserEntry,
};
use crate::value::field;

const PLAN_MODE_EMISSIONS: usize = 2;

#[derive(Default)]
struct OwnedPayloads {
    entries: usize,
    tools: usize,
    questions: usize,
    results: usize,
    signals: usize,
    plan_uuids: usize,
}

pub struct ReviewMiningIndex {
    fingerprint: [u8; 32],
    pub(super) entries: Vec<Entry>,
    pub(super) dependencies: ReviewDependencies,
    pub(super) uses: HashMap<String, Vec<(usize, ToolUseBlock)>>,
    pub(super) assistants: BTreeSet<usize>,
    pub(super) users: BTreeSet<usize>,
    pub(super) edits: BTreeSet<usize>,
    pub(super) question_refs: HashMap<usize, Vec<String>>,
    pub(super) result_facts: HashMap<usize, Vec<ResultFacts>>,
    pub(super) plan_seen: HashSet<String>,
    pub(super) pending: BTreeSet<usize>,
    pub(super) signals: [BTreeMap<usize, Vec<MiningSignal>>; 7],
    owned: OwnedPayloads,
}

fn compact_use(tool: &ToolUseBlock) -> ToolUseBlock {
    ToolUseBlock {
        id: tool.id.clone(),
        name: tool.name.clone(),
        run_in_background: tool.run_in_background,
        subagent_type: tool.subagent_type.clone(),
        file_path: tool.file_path.clone(),
        questions: tool.questions.clone(),
        input: sonic_rs::json!(null),
    }
}

fn question_payload(result: &ToolResultBlock) -> bool {
    !result.is_error
        && (result
            .tool_use_result
            .as_ref()
            .is_some_and(|payload| field(payload, "answers").is_some())
            || result.content.starts_with(ANSWERED_PREFIX)
                && result.content.ends_with(ANSWERED_TRAILER)
                && result.content.len() >= ANSWERED_PREFIX.len() + ANSWERED_TRAILER.len())
}

pub(super) struct ResultFacts {
    pub embedded: Option<String>,
    pub interrupt: bool,
}

fn compact_result(result: &ToolResultBlock) -> Option<(ToolResultBlock, ResultFacts)> {
    let denial = result.denial_kind.as_deref() == Some(DENIAL_KIND_USER_REJECTED);
    let interrupt = interrupt_marker(&result.content).is_some();
    if !denial && !interrupt && !question_payload(result) {
        return None;
    }
    let facts = ResultFacts {
        embedded: denial
            .then(|| embedded_user_text(&result.content))
            .flatten(),
        interrupt,
    };
    Some((
        ToolResultBlock {
            tool_use_id: result.tool_use_id.clone(),
            content: String::new(),
            is_error: result.is_error,
            is_async: result.is_async,
            tool_use_result: None,
            denial_kind: denial.then(|| DENIAL_KIND_USER_REJECTED.to_owned()),
        },
        facts,
    ))
}

fn compact_entry(entry: &Entry) -> (Entry, Vec<ResultFacts>) {
    match entry {
        Entry::User(user) => {
            let mut blocks = vec![ContentBlock::Text(user.content.text())];
            let mut facts = Vec::new();
            for block in user.blocks() {
                match block {
                    ContentBlock::ToolUse(tool) => {
                        blocks.push(ContentBlock::ToolUse(compact_use(tool)))
                    }
                    ContentBlock::ToolResult(result) => {
                        if let Some((result, fact)) = compact_result(result) {
                            blocks.push(ContentBlock::ToolResult(result));
                            facts.push(fact);
                        }
                    }
                    _ => {}
                }
            }
            (
                Entry::User(UserEntry {
                    meta: user.meta.clone(),
                    content: UserContent::Blocks(blocks),
                    prompt_id: None,
                    prompt_source: None,
                    queue_priority: None,
                    image_paste_ids: None,
                    source_tool_use_id: None,
                    source_tool_assistant_uuid: None,
                    mcp_meta: None,
                    permission_mode: None,
                    interrupted_message_id: None,
                }),
                facts,
            )
        }
        Entry::Assistant(assistant) => (
            Entry::Assistant(AssistantEntry {
                meta: assistant.meta.clone(),
                model: String::new(),
                blocks: assistant
                    .blocks
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::ToolUse(tool) => {
                            Some(ContentBlock::ToolUse(compact_use(tool)))
                        }
                        _ => None,
                    })
                    .collect(),
                stop_reason: None,
                usage: None,
                request_id: None,
                forked_from: None,
                attribution: None,
                api_error: None,
            }),
            Vec::new(),
        ),
        Entry::Mode(mode) => (
            Entry::Mode(ModeEntry {
                session_id: mode.session_id.clone(),
                channel: mode.channel,
                value: mode.value.clone(),
            }),
            Vec::new(),
        ),
        _ => (
            Entry::Other(OtherEntry {
                ty: String::new(),
                raw: sonic_rs::json!(null),
            }),
            Vec::new(),
        ),
    }
}

impl ReviewMiningIndex {
    pub fn new(spec: &CompiledMiningSpec) -> MiningResult<Self> {
        if spec
            .review
            .surfaces
            .iter()
            .any(|surface| surface != "typed")
        {
            return Err(MiningError::Snapshot(SnapshotError::new(
                Status::Incomplete,
                "registered review mining requires typed review surfaces",
            )));
        }
        if spec
            .review
            .callable_formats
            .iter()
            .any(|format| !format.bounded())
        {
            return Err(MiningError::Snapshot(SnapshotError::new(
                Status::Incomplete,
                "mining callback is not registered as bounded",
            )));
        }
        Ok(Self {
            fingerprint: spec.fingerprint(),
            entries: Vec::new(),
            dependencies: ReviewDependencies::default(),
            uses: HashMap::new(),
            assistants: BTreeSet::new(),
            users: BTreeSet::new(),
            edits: BTreeSet::new(),
            question_refs: HashMap::new(),
            result_facts: HashMap::new(),
            plan_seen: HashSet::new(),
            pending: BTreeSet::new(),
            signals: Default::default(),
            owned: OwnedPayloads::default(),
        })
    }

    pub fn event_count(&self) -> usize {
        self.entries.len()
    }

    pub(crate) fn pending_count(&self) -> usize {
        self.pending.len()
    }

    pub fn accounted_bytes(&self) -> usize {
        size_of::<Self>()
            + self.entries.capacity() * size_of::<Entry>()
            + self.dependencies.accounted_bytes()
            - size_of::<ReviewDependencies>()
            + self.uses.capacity() * size_of::<(String, Vec<(usize, ToolUseBlock)>)>()
            + (self.assistants.len() + self.users.len() + self.edits.len() + self.pending.len())
                * size_of::<usize>()
            + self.question_refs.capacity() * size_of::<(usize, Vec<String>)>()
            + self.result_facts.capacity() * size_of::<(usize, Vec<ResultFacts>)>()
            + self.plan_seen.capacity() * size_of::<String>()
            + self
                .signals
                .iter()
                .map(|rows| rows.len() * size_of::<(usize, Vec<MiningSignal>)>())
                .sum::<usize>()
            + self.owned.entries
            + self.owned.tools
            + self.owned.questions
            + self.owned.results
            + self.owned.signals
            + self.owned.plan_uuids
    }

    #[cfg(test)]
    pub(crate) fn audited_bytes(&self) -> usize {
        let entries = self
            .entries
            .iter()
            .map(|entry| {
                let charge = entry_charge(entry);
                charge.owned_capacity_bytes + charge.opaque_dom_accounted_bytes
            })
            .sum::<usize>();
        let tools = self
            .uses
            .iter()
            .map(|(id, versions)| {
                id.capacity()
                    + versions.capacity() * size_of::<(usize, ToolUseBlock)>()
                    + versions
                        .iter()
                        .map(|(_, tool)| {
                            let charge = tool_use_charge(tool);
                            charge.owned_capacity_bytes + charge.opaque_dom_accounted_bytes
                        })
                        .sum::<usize>()
            })
            .sum::<usize>();
        let questions = self
            .question_refs
            .values()
            .map(|ids| {
                ids.capacity() * size_of::<String>()
                    + ids.iter().map(String::capacity).sum::<usize>()
            })
            .sum::<usize>();
        let results = self
            .result_facts
            .values()
            .map(|facts| {
                facts.capacity() * size_of::<ResultFacts>()
                    + facts
                        .iter()
                        .filter_map(|fact| fact.embedded.as_ref())
                        .map(String::capacity)
                        .sum::<usize>()
            })
            .sum::<usize>();
        let signals = self
            .signals
            .iter()
            .flat_map(|map| map.values())
            .map(|rows| {
                rows.capacity() * size_of::<MiningSignal>()
                    + rows
                        .iter()
                        .map(|row| row.accounted_bytes() - size_of::<MiningSignal>())
                        .sum::<usize>()
            })
            .sum::<usize>();
        self.accounted_bytes()
            - self.owned.entries
            - self.owned.tools
            - self.owned.questions
            - self.owned.results
            - self.owned.signals
            - self.owned.plan_uuids
            + entries
            + tools
            + questions
            + results
            + signals
            + self.plan_seen.iter().map(String::capacity).sum::<usize>()
    }

    fn verify_spec(&self, spec: &CompiledMiningSpec) -> MiningResult<()> {
        if self.fingerprint != spec.fingerprint() {
            return Err(SnapshotError::new(Status::Changed, "review mining policy changed").into());
        }
        Ok(())
    }

    pub(crate) fn append_bound(
        &self,
        entry: &Entry,
        spec: &CompiledMiningSpec,
    ) -> MiningResult<usize> {
        self.verify_spec(spec)?;
        use crate::snapshot_ledger::{map_reallocation_bound, vec_reallocation_bound};
        let followup = match entry {
            Entry::User(user) => {
                let text = user.content.text();
                if text.trim().is_empty() {
                    Followup::Blank
                } else if crate::protocol::is_bare_interrupt_marker(&text) {
                    Followup::BareMarker
                } else if structural_re(spec).is_match(&text) {
                    Followup::Structural
                } else {
                    Followup::Correction
                }
            }
            _ => Followup::Blank,
        };
        let (dependencies, affected) = self.dependencies.append_bound(entry, followup);
        let charge = entry_charge(entry);
        let tools = entry.tool_uses().count();
        let tool_versions = entry
            .tool_uses()
            .map(|tool| {
                self.uses.get(&tool.id).map_or(
                    4usize.max(tools) * size_of::<(usize, ToolUseBlock)>(),
                    |versions| vec_reallocation_bound(versions, tools),
                )
            })
            .sum::<usize>();
        Ok(dependencies
            + tool_versions
            + (charge.owned_capacity_bytes + charge.opaque_dom_accounted_bytes) * 12
            + vec_reallocation_bound(&self.entries, 1)
            + map_reallocation_bound(&self.uses, tools)
            + map_reallocation_bound(&self.question_refs, 1)
            + map_reallocation_bound(&self.result_facts, 1)
            + (affected + 8) * 4 * size_of::<usize>()
            + 4096)
    }

    pub fn append(
        &mut self,
        entries: &[&Entry],
        spec: &CompiledMiningSpec,
    ) -> MiningResult<ReviewChanges> {
        self.verify_spec(spec)?;
        let start = self.entries.len();
        let structural = structural_re(spec);
        let changes = self.dependencies.append_parsed(
            entries,
            |text| structural.is_match(text),
            spec.detectors.contains(DETECTOR_INTERRUPT),
            spec.detectors.contains(DETECTOR_DENIAL),
            spec.detectors.contains(DETECTOR_PLAN_REENTRY),
        )?;
        for (offset, entry) in entries.iter().enumerate() {
            let index = start + offset;
            for tool in entry.tool_uses() {
                if matches_names(&tool.name, &spec.edit_tools) {
                    self.edits.insert(index);
                }
                let tool = compact_use(tool);
                let charge = tool_use_charge(&tool);
                self.owned.tools += charge.owned_capacity_bytes + charge.opaque_dom_accounted_bytes;
                let versions = match self.uses.entry(tool.id.clone()) {
                    std::collections::hash_map::Entry::Vacant(slot) => {
                        self.owned.tools += slot.key().capacity();
                        slot.insert(Vec::new())
                    }
                    std::collections::hash_map::Entry::Occupied(slot) => slot.into_mut(),
                };
                let old_capacity = versions.capacity();
                versions.push((index, tool));
                self.owned.tools +=
                    (versions.capacity() - old_capacity) * size_of::<(usize, ToolUseBlock)>();
            }
            match entry {
                Entry::Assistant(_) => {
                    self.assistants.insert(index);
                }
                Entry::User(user) => {
                    if !user.content.text().trim().is_empty() {
                        self.users.insert(index);
                    }
                    let questions = user
                        .tool_results()
                        .filter(|result| question_payload(result))
                        .map(|result| result.tool_use_id.clone())
                        .collect::<Vec<_>>();
                    if !questions.is_empty() {
                        self.owned.questions += questions.capacity() * size_of::<String>()
                            + questions.iter().map(String::capacity).sum::<usize>();
                        self.question_refs.insert(index, questions);
                    }
                }
                _ => {}
            }
            let (entry, facts) = compact_entry(entry);
            if !facts.is_empty() {
                self.owned.results += facts.capacity() * size_of::<ResultFacts>()
                    + facts
                        .iter()
                        .filter_map(|fact| fact.embedded.as_ref())
                        .map(String::capacity)
                        .sum::<usize>();
                self.result_facts.insert(index, facts);
            }
            let charge = entry_charge(&entry);
            self.owned.entries += charge.owned_capacity_bytes + charge.opaque_dom_accounted_bytes;
            self.entries.push(entry);
        }
        self.pending.extend(start..self.entries.len());
        self.pending.extend(changes.revisited.iter().copied());
        Ok(changes)
    }

    pub fn last_result(&self, id: &str) -> Option<usize> {
        self.dependencies.last_result(id)
    }

    pub fn tool_position(&self, id: &str) -> Option<usize> {
        self.uses
            .get(id)
            .and_then(|rows| rows.last().map(|(index, _)| *index))
    }

    pub(crate) fn truncate_bound(&self, first: usize, ids: &HashSet<String>) -> usize {
        self.dependencies.truncate_bound(first, ids)
    }

    pub fn truncate(
        &mut self,
        first: usize,
        spec: &CompiledMiningSpec,
    ) -> MiningResult<ReviewChanges> {
        self.verify_spec(spec)?;
        let changes = self.dependencies.truncate(first)?;
        self.pending.split_off(&first);
        self.assistants.split_off(&first);
        self.users.split_off(&first);
        self.edits.split_off(&first);
        for (detector, signals) in self.signals.iter_mut().enumerate() {
            for rows in signals.split_off(&first).into_values() {
                self.owned.signals -= rows.capacity() * size_of::<MiningSignal>()
                    + rows
                        .iter()
                        .map(|row| row.accounted_bytes() - size_of::<MiningSignal>())
                        .sum::<usize>();
                if detector == PLAN_MODE_EMISSIONS {
                    for row in rows {
                        if let Some(uuid) = self.plan_seen.take(&row.event_uuid) {
                            self.owned.plan_uuids -= uuid.capacity();
                        }
                    }
                }
            }
        }
        while self.entries.len() > first {
            let index = self.entries.len() - 1;
            let entry = self.entries.pop().expect("committed mining entry");
            let charge = entry_charge(&entry);
            self.owned.entries -= charge.owned_capacity_bytes + charge.opaque_dom_accounted_bytes;
            for tool in entry.tool_uses() {
                let versions = self
                    .uses
                    .get_mut(&tool.id)
                    .expect("committed tool versions");
                let (event, old) = versions.pop().expect("committed tool version");
                assert_eq!(event, index);
                let charge = tool_use_charge(&old);
                self.owned.tools -= charge.owned_capacity_bytes + charge.opaque_dom_accounted_bytes;
                if versions.is_empty() {
                    let (id, versions) = self
                        .uses
                        .remove_entry(&tool.id)
                        .expect("empty tool versions");
                    self.owned.tools -=
                        id.capacity() + versions.capacity() * size_of::<(usize, ToolUseBlock)>();
                }
            }
            if let Some(ids) = self.question_refs.remove(&index) {
                self.owned.questions -= ids.capacity() * size_of::<String>()
                    + ids.iter().map(String::capacity).sum::<usize>();
            }
            if let Some(facts) = self.result_facts.remove(&index) {
                self.owned.results -= facts.capacity() * size_of::<ResultFacts>()
                    + facts
                        .iter()
                        .filter_map(|fact| fact.embedded.as_ref())
                        .map(String::capacity)
                        .sum::<usize>();
            }
        }
        self.pending.extend(changes.revisited.iter().copied());
        Ok(changes)
    }

    pub fn required_payloads(&self, spec: &CompiledMiningSpec) -> MiningResult<Vec<usize>> {
        self.verify_spec(spec)?;
        if !spec.detectors.contains(DETECTOR_ASK_USER_QUESTION) {
            return Ok(Vec::new());
        }
        Ok(self
            .pending
            .iter()
            .copied()
            .filter(|index| {
                self.question_refs.get(index).is_some_and(|ids| {
                    ids.iter().any(|id| {
                        self.uses
                            .get(id)
                            .and_then(|versions| versions.last())
                            .is_some_and(|(_, tool)| tool.name == "AskUserQuestion")
                    })
                })
            })
            .collect())
    }

    pub fn process(
        &mut self,
        spec: &CompiledMiningSpec,
        payloads: &HashMap<usize, &Entry>,
        limits: WorkLimits,
        cancel: &Cancellation,
        usage: &mut MiningUsage,
    ) -> MiningResult<()> {
        *usage = MiningUsage::default();
        self.verify_spec(spec)?;
        if self.pending.is_empty() {
            return Ok(());
        }
        if self
            .required_payloads(spec)?
            .iter()
            .any(|event| !payloads.contains_key(event))
        {
            return Err(MiningError::Snapshot(SnapshotError::new(
                Status::Incomplete,
                "review question payloads are not materialized",
            )));
        }
        let rows = super::mine_indexed(self, spec, payloads, limits, cancel, usage)?;
        for (detector, signals) in self.signals.iter_mut().enumerate() {
            if detector != PLAN_MODE_EMISSIONS {
                for event in &self.pending {
                    if let Some(rows) = signals.remove(event) {
                        self.owned.signals -= rows.capacity() * size_of::<MiningSignal>()
                            + rows
                                .iter()
                                .map(|row| row.accounted_bytes() - size_of::<MiningSignal>())
                                .sum::<usize>();
                    }
                }
            }
        }
        for row in rows {
            let detector = match row.detector.as_str() {
                DETECTOR_TRANSCRIPT_MESSAGE => 0,
                DETECTOR_EXIT_PLAN_REJECTION => 1,
                DETECTOR_PLAN_REENTRY => PLAN_MODE_EMISSIONS,
                DETECTOR_DENIAL => 3,
                DETECTOR_INTERRUPT => 4,
                DETECTOR_REVIEW_COMMENT => 5,
                DETECTOR_ASK_USER_QUESTION => 6,
                _ => unreachable!(),
            };
            if row.detector == DETECTOR_PLAN_REENTRY && !self.plan_seen.contains(&row.event_uuid) {
                let uuid = row.event_uuid.clone();
                self.owned.plan_uuids += uuid.capacity();
                self.plan_seen.insert(uuid);
            }
            self.owned.signals += row.accounted_bytes() - size_of::<MiningSignal>();
            let rows = self.signals[detector].entry(row.event_index).or_default();
            let old_capacity = rows.capacity();
            rows.push(row);
            self.owned.signals += (rows.capacity() - old_capacity) * size_of::<MiningSignal>();
        }
        self.pending.clear();
        Ok(())
    }

    pub fn signals(&self) -> MiningResult<impl Iterator<Item = &MiningSignal>> {
        if !self.pending.is_empty() {
            return Err(MiningError::Snapshot(SnapshotError::new(
                Status::Incomplete,
                "review mining has unprocessed input",
            )));
        }
        Ok(self
            .signals
            .iter()
            .flat_map(|detector| detector.values())
            .flatten())
    }

    pub(super) fn text(&self, index: usize) -> &str {
        match &self.entries[index] {
            Entry::User(UserEntry {
                content: UserContent::Blocks(blocks),
                ..
            }) => {
                let ContentBlock::Text(text) = &blocks[0] else {
                    unreachable!()
                };
                text
            }
            _ => "",
        }
    }
}
