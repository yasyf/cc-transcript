use std::collections::{BTreeSet, HashMap, HashSet};
use std::mem::size_of;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::protocol::{
    embedded_user_text, interrupt_marker, is_bare_interrupt_marker, DENIAL_KIND_USER_REJECTED,
};
use crate::snapshot::{SnapshotError, Status};
use crate::types::Entry;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Followup {
    Blank,
    BareMarker,
    Structural,
    Correction,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewEventDependencies {
    pub index: usize,
    pub tool_uses: Vec<String>,
    pub tool_results: Vec<String>,
    pub followup: Followup,
    pub waits_for_correction: bool,
    pub plan_mode: bool,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewFollowupTargets {
    pub first: Option<usize>,
    pub correction: Option<usize>,
}

#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct ReviewUndo {
    uses: Vec<String>,
    results: Vec<String>,
    first: Vec<usize>,
    corrections: Vec<usize>,
    plans: Vec<usize>,
    targets: Vec<(usize, ReviewFollowupTargets)>,
    waits: bool,
    plan: bool,
}

impl ReviewUndo {
    fn owned_bytes(&self) -> usize {
        (self.uses.capacity() + self.results.capacity()) * size_of::<String>()
            + self
                .uses
                .iter()
                .chain(&self.results)
                .map(String::capacity)
                .sum::<usize>()
            + (self.first.capacity() + self.corrections.capacity() + self.plans.capacity())
                * size_of::<usize>()
            + self.targets.capacity() * size_of::<(usize, ReviewFollowupTargets)>()
    }
}

#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewDependencies {
    events: usize,
    undo: Vec<ReviewUndo>,
    undo_bytes: usize,
    results: HashMap<String, Vec<usize>>,
    first_followups: Vec<usize>,
    corrections: Vec<usize>,
    plans: Vec<usize>,
    followups: HashMap<usize, ReviewFollowupTargets>,
    plan_users: HashMap<usize, usize>,
    #[serde(skip)]
    accounted: AtomicUsize,
}

#[derive(Debug, PartialEq, Eq)]
pub struct ReviewChanges {
    pub start: usize,
    pub end: usize,
    pub revisited: BTreeSet<usize>,
}

impl ReviewDependencies {
    pub(crate) fn append_bound(&self, entry: &Entry, followup: Followup) -> (usize, usize) {
        use crate::snapshot_ledger::{map_reallocation_bound, vec_reallocation_bound};
        let uses = entry.tool_uses().count();
        let results = match entry {
            Entry::User(user) => user.tool_results().count(),
            _ => 0,
        };
        let first = usize::from(followup == Followup::Structural) * self.first_followups.len();
        let corrections = usize::from(followup == Followup::Correction) * self.corrections.len();
        let plans = usize::from(followup != Followup::Blank) * self.plans.len();
        let mut affected = first + corrections + plans;
        for tool in entry.tool_uses() {
            affected += self.results.get(&tool.id).map_or(0, Vec::len);
        }
        let mut bytes = map_reallocation_bound(&self.results, results)
            + vec_reallocation_bound(&self.first_followups, 1)
            + vec_reallocation_bound(&self.corrections, 1)
            + vec_reallocation_bound(&self.plans, 1)
            + map_reallocation_bound(&self.followups, 1)
            + map_reallocation_bound(&self.plan_users, plans)
            + (affected + uses + results + 1) * 12 * size_of::<usize>()
            + vec_reallocation_bound(&self.undo, 1)
            + (uses + results) * size_of::<String>();
        bytes += entry.tool_uses().map(|tool| tool.id.len()).sum::<usize>();
        if let Entry::User(user) = entry {
            for result in user.tool_results() {
                bytes += result.tool_use_id.len() * 2;
                bytes += self
                    .results
                    .get(&result.tool_use_id)
                    .map_or(4usize.max(results) * size_of::<usize>(), |indices| {
                        vec_reallocation_bound(indices, results)
                    });
            }
        }
        (bytes, affected)
    }

    pub fn last_result(&self, id: &str) -> Option<usize> {
        self.results.get(id).and_then(|rows| rows.last().copied())
    }

    pub(crate) fn truncate_bound(&self, first: usize, ids: &HashSet<String>) -> usize {
        use crate::snapshot_ledger::{
            map_reallocation_bound, set_reallocation_bound, vec_reallocation_bound,
        };
        let undo = &self.undo[first..];
        let uses = undo.iter().map(|undo| undo.uses.len()).sum::<usize>();
        let targets = undo.iter().map(|undo| undo.targets.len()).sum::<usize>();
        let plans = undo.iter().map(|undo| undo.plans.len()).sum::<usize>();
        let firsts = undo.iter().map(|undo| undo.first.len()).sum::<usize>();
        let corrections = undo
            .iter()
            .map(|undo| undo.corrections.len())
            .sum::<usize>();
        let results = ids
            .iter()
            .map(|id| self.results.get(id).map_or(0, Vec::len))
            .sum::<usize>();
        set_reallocation_bound(&HashSet::<String>::new(), uses)
            + map_reallocation_bound(&self.followups, targets)
            + vec_reallocation_bound(&self.first_followups, firsts)
            + vec_reallocation_bound(&self.corrections, corrections)
            + vec_reallocation_bound(&self.plans, plans)
            + (results + targets + plans) * 3 * size_of::<usize>()
    }

    pub fn truncate(&mut self, first: usize) -> Result<ReviewChanges, SnapshotError> {
        if first > self.events {
            return Err(SnapshotError::new(
                Status::InvalidRequest,
                "review truncate exceeds committed sequence",
            ));
        }
        let old_end = self.events;
        let mut accounted = self.accounted_bytes();
        let mut revisited = BTreeSet::new();
        let uses = self.undo[first..]
            .iter()
            .map(|undo| undo.uses.len())
            .sum::<usize>();
        let mut removed_uses = HashSet::with_capacity(uses);
        let old_tables = self.results.capacity() * size_of::<(String, Vec<usize>)>()
            + self.followups.capacity() * size_of::<(usize, ReviewFollowupTargets)>()
            + self.plan_users.capacity() * size_of::<(usize, usize)>();
        while self.events > first {
            self.events -= 1;
            let event = self.events;
            let undo = self.undo.pop().expect("committed dependency undo");
            let bytes = undo.owned_bytes();
            self.undo_bytes -= bytes;
            accounted -= bytes;
            removed_uses.extend(undo.uses);
            if undo.waits {
                assert_eq!(self.first_followups.pop(), Some(event));
                assert_eq!(self.corrections.pop(), Some(event));
                self.followups.remove(&event);
            }
            if undo.plan {
                assert_eq!(self.plans.pop(), Some(event));
            }
            for id in &undo.results {
                let rows = self
                    .results
                    .get_mut(id)
                    .expect("committed result positions");
                assert_eq!(rows.pop(), Some(event));
                if rows.is_empty() {
                    let (id, rows) = self
                        .results
                        .remove_entry(id)
                        .expect("empty result positions");
                    accounted -= id.capacity() + rows.capacity() * size_of::<usize>();
                }
            }
            for (index, targets) in undo.targets {
                self.followups.insert(index, targets);
                if index < first {
                    revisited.insert(index);
                }
            }
            for &index in &undo.plans {
                self.plan_users.remove(&index);
                if index < first {
                    revisited.insert(index);
                }
            }
            let old = self.first_followups.capacity()
                + self.corrections.capacity()
                + self.plans.capacity();
            self.first_followups.extend(undo.first);
            self.corrections.extend(undo.corrections);
            self.plans.extend(undo.plans);
            accounted += (self.first_followups.capacity()
                + self.corrections.capacity()
                + self.plans.capacity()
                - old)
                * size_of::<usize>();
        }
        for id in removed_uses {
            if let Some(rows) = self.results.get(&id) {
                revisited.extend(rows.iter().copied());
            }
        }
        accounted = accounted - old_tables
            + self.results.capacity() * size_of::<(String, Vec<usize>)>()
            + self.followups.capacity() * size_of::<(usize, ReviewFollowupTargets)>()
            + self.plan_users.capacity() * size_of::<(usize, usize)>();
        self.accounted.store(accounted, Ordering::Relaxed);
        Ok(ReviewChanges {
            start: first,
            end: old_end,
            revisited,
        })
    }

    pub fn event_count(&self) -> usize {
        self.events
    }

    pub fn followup(&self, event: usize) -> Option<ReviewFollowupTargets> {
        self.followups.get(&event).copied()
    }

    pub fn plan_user(&self, event: usize) -> Option<usize> {
        self.plan_users.get(&event).copied()
    }

    pub fn accounted_bytes(&self) -> usize {
        let cached = self.accounted.load(Ordering::Relaxed);
        if cached != 0 {
            return cached;
        }
        let bytes = size_of::<Self>()
            + self.undo.capacity() * size_of::<ReviewUndo>()
            + self.undo_bytes
            + self.results.capacity() * size_of::<(String, Vec<usize>)>()
            + self
                .results
                .iter()
                .map(|(id, indices)| id.capacity() + indices.capacity() * size_of::<usize>())
                .sum::<usize>()
            + self.first_followups.capacity() * size_of::<usize>()
            + self.corrections.capacity() * size_of::<usize>()
            + self.plans.capacity() * size_of::<usize>()
            + self.followups.capacity() * size_of::<(usize, ReviewFollowupTargets)>()
            + self.plan_users.capacity() * size_of::<(usize, usize)>();
        self.accounted.store(bytes, Ordering::Relaxed);
        bytes
    }

    pub fn append_parsed(
        &mut self,
        entries: &[&Entry],
        mut structural: impl FnMut(&str) -> bool,
        interruptions: bool,
        denials: bool,
        plans: bool,
    ) -> Result<ReviewChanges, SnapshotError> {
        let events = entries
            .iter()
            .enumerate()
            .map(|(offset, entry)| {
                let mut event = ReviewEventDependencies {
                    index: self.events + offset,
                    tool_uses: entry.tool_uses().map(|tool| tool.id.clone()).collect(),
                    tool_results: Vec::new(),
                    followup: Followup::Blank,
                    waits_for_correction: false,
                    plan_mode: plans && matches!(entry, Entry::Mode(mode) if mode.value == "plan"),
                };
                if let Entry::User(user) = entry {
                    let text = user.content.text();
                    event.followup = if text.trim().is_empty() {
                        Followup::Blank
                    } else if is_bare_interrupt_marker(&text) {
                        Followup::BareMarker
                    } else if structural(&text) {
                        Followup::Structural
                    } else {
                        Followup::Correction
                    };
                    for result in user.tool_results() {
                        event.tool_results.push(result.tool_use_id.clone());
                        event.waits_for_correction |= interruptions
                            && interrupt_marker(&result.content).is_some()
                            || denials
                                && result.denial_kind.as_deref() == Some(DENIAL_KIND_USER_REJECTED)
                                && embedded_user_text(&result.content)
                                    .is_none_or(|text| text.is_empty());
                    }
                }
                event
            })
            .collect::<Vec<_>>();
        self.append(&events)
    }

    pub fn append(
        &mut self,
        events: &[ReviewEventDependencies],
    ) -> Result<ReviewChanges, SnapshotError> {
        if events
            .iter()
            .enumerate()
            .any(|(offset, event)| self.events.checked_add(offset) != Some(event.index))
        {
            return Err(SnapshotError::new(
                Status::InvalidRequest,
                "review dependency events must extend the committed sequence",
            ));
        }
        let start = self.events;
        let mut accounted = self.accounted_bytes();
        let old_undo_capacity = self.undo.capacity();
        let old_undo_bytes = self.undo_bytes;
        let old_results_capacity = self.results.capacity();
        let old_first_capacity = self.first_followups.capacity();
        let old_corrections_capacity = self.corrections.capacity();
        let old_plans_capacity = self.plans.capacity();
        let old_followups_capacity = self.followups.capacity();
        let old_plan_users_capacity = self.plan_users.capacity();
        let mut revisited = BTreeSet::new();
        for event in events {
            let mut undo = ReviewUndo {
                uses: event.tool_uses.clone(),
                results: Vec::new(),
                waits: event.waits_for_correction,
                plan: event.plan_mode,
                ..ReviewUndo::default()
            };
            for id in &event.tool_uses {
                if let Some(indices) = self.results.get(id) {
                    revisited.extend(indices.iter().copied().filter(|index| *index < start));
                }
            }
            if event.followup != Followup::Blank {
                undo.plans.extend(self.plans.drain(..));
                for &index in &undo.plans {
                    self.plan_users.insert(index, event.index);
                    if index < start {
                        revisited.insert(index);
                    }
                }
            }
            match event.followup {
                Followup::Structural => {
                    undo.first.extend(self.first_followups.drain(..));
                    for &index in &undo.first {
                        undo.targets.push((index, self.followups[&index]));
                        self.followups
                            .get_mut(&index)
                            .expect("pending followup anchor")
                            .first = Some(event.index);
                        if index < start {
                            revisited.insert(index);
                        }
                    }
                }
                Followup::Correction => {
                    undo.corrections.extend(self.corrections.drain(..));
                    for &index in &undo.corrections {
                        undo.targets.push((index, self.followups[&index]));
                        let targets = self
                            .followups
                            .get_mut(&index)
                            .expect("pending correction anchor");
                        targets.first.get_or_insert(event.index);
                        targets.correction = Some(event.index);
                        if index < start {
                            revisited.insert(index);
                        }
                    }
                    undo.first.extend(self.first_followups.drain(..));
                }
                Followup::Blank | Followup::BareMarker => {}
            }
            for id in &event.tool_results {
                let indices = match self.results.entry(id.clone()) {
                    std::collections::hash_map::Entry::Occupied(entry) => entry.into_mut(),
                    std::collections::hash_map::Entry::Vacant(entry) => {
                        accounted += entry.key().capacity();
                        entry.insert(Vec::new())
                    }
                };
                if indices.last() != Some(&event.index) {
                    let old_capacity = indices.capacity();
                    indices.push(event.index);
                    undo.results.push(id.clone());
                    accounted += (indices.capacity() - old_capacity) * size_of::<usize>();
                }
            }
            if event.waits_for_correction {
                self.followups
                    .insert(event.index, ReviewFollowupTargets::default());
                self.first_followups.push(event.index);
                self.corrections.push(event.index);
            }
            if event.plan_mode {
                self.plans.push(event.index);
            }
            self.undo_bytes += undo.owned_bytes();
            self.undo.push(undo);
        }
        self.events += events.len();
        accounted += self.undo_bytes - old_undo_bytes
            + (self.undo.capacity() - old_undo_capacity) * size_of::<ReviewUndo>();
        accounted += (self.results.capacity() - old_results_capacity)
            * size_of::<(String, Vec<usize>)>()
            + (self.first_followups.capacity() - old_first_capacity + self.corrections.capacity()
                - old_corrections_capacity
                + self.plans.capacity()
                - old_plans_capacity)
                * size_of::<usize>();
        accounted += (self.followups.capacity() - old_followups_capacity)
            * size_of::<(usize, ReviewFollowupTargets)>()
            + (self.plan_users.capacity() - old_plan_users_capacity) * size_of::<(usize, usize)>();
        self.accounted.store(accounted, Ordering::Relaxed);
        Ok(ReviewChanges {
            start,
            end: self.events,
            revisited,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(index: usize, followup: Followup) -> ReviewEventDependencies {
        ReviewEventDependencies {
            index,
            tool_uses: Vec::new(),
            tool_results: Vec::new(),
            followup,
            waits_for_correction: false,
            plan_mode: false,
        }
    }

    #[test]
    fn suffix_rollback_cached_charge_matches_large_unique_result_table_audit() {
        let rows = (0..256)
            .map(|index| {
                let mut row = event(index, Followup::Blank);
                row.tool_results.push(format!("unique-{index}"));
                row
            })
            .collect::<Vec<_>>();
        for first in [0, 1, 127, 255, 256] {
            let mut index = ReviewDependencies::default();
            index.append(&rows).unwrap();
            index.truncate(first).unwrap();
            let cached = index.accounted_bytes();
            index.accounted.store(0, Ordering::Relaxed);
            assert_eq!(cached, index.accounted_bytes(), "cut={first}");
        }
    }

    #[test]
    fn suffix_undo_restores_drained_frontiers_targets_and_result_stacks() {
        let mut rows = Vec::new();
        let mut anchor = event(0, Followup::Blank);
        anchor.waits_for_correction = true;
        anchor.plan_mode = true;
        anchor.tool_results = vec!["shared".into(), "shared".into()];
        rows.push(anchor);
        rows.push(event(1, Followup::Structural));
        rows.push(event(2, Followup::Correction));
        let mut tail = event(3, Followup::Blank);
        tail.tool_uses = vec!["shared".into()];
        tail.tool_results = vec!["shared".into()];
        tail.waits_for_correction = true;
        tail.plan_mode = true;
        rows.push(tail);
        rows.push(event(4, Followup::Correction));
        for first in 0..=rows.len() {
            let mut index = ReviewDependencies::default();
            index.append(&rows).unwrap();
            let changed = index.truncate(first).unwrap();
            assert_eq!((changed.start, changed.end), (first, rows.len()));
            assert!(changed.revisited.iter().all(|event| *event < first));
            let mut cold = ReviewDependencies::default();
            cold.append(&rows[..first]).unwrap();
            assert_eq!(index.results, cold.results);
            assert_eq!(index.first_followups, cold.first_followups);
            assert_eq!(index.corrections, cold.corrections);
            assert_eq!(index.plans, cold.plans);
            assert_eq!(index.followups, cold.followups);
            assert_eq!(index.plan_users, cold.plan_users);
            assert_eq!(
                index.undo_bytes,
                index.undo.iter().map(ReviewUndo::owned_bytes).sum()
            );
            let cached = index.accounted_bytes();
            index.accounted.store(0, Ordering::Relaxed);
            assert_eq!(index.accounted_bytes(), cached);
            index.append(&rows[first..]).unwrap();
            assert_eq!(index.last_result("shared"), Some(3));
        }
    }

    #[test]
    fn incremental_charge_matches_cold_capacity_audit_after_growth_and_resolution() {
        let mut dependencies = ReviewDependencies::default();
        for index in 0..256 {
            let mut next = event(index, Followup::Blank);
            next.tool_results = vec![format!("id-{index}").repeat(index % 7 + 1), "shared".into()];
            next.waits_for_correction = true;
            next.plan_mode = true;
            dependencies.append(&[next]).unwrap();
            let cached = dependencies.accounted_bytes();
            dependencies.accounted.store(0, Ordering::Relaxed);
            assert_eq!(dependencies.accounted_bytes(), cached);
        }
        dependencies
            .append(&[event(256, Followup::Correction)])
            .unwrap();
        let cached = dependencies.accounted_bytes();
        dependencies.accounted.store(0, Ordering::Relaxed);
        assert_eq!(dependencies.accounted_bytes(), cached);
    }

    #[test]
    fn old_marker_remains_pending_across_arbitrarily_many_non_corrections() {
        let mut dependencies = ReviewDependencies::default();
        let mut marker = event(0, Followup::Blank);
        marker.waits_for_correction = true;
        dependencies.append(&[marker]).unwrap();
        for index in 1..=4096 {
            let followup = if index % 2 == 0 {
                Followup::Structural
            } else {
                Followup::BareMarker
            };
            let changes = dependencies.append(&[event(index, followup)]).unwrap();
            assert_eq!(changes.revisited.contains(&0), index == 2);
        }
        let changes = dependencies
            .append(&[event(4097, Followup::Correction)])
            .unwrap();
        assert_eq!(changes.revisited, BTreeSet::from([0]));
        assert_eq!(
            dependencies.followup(0),
            Some(ReviewFollowupTargets {
                first: Some(2),
                correction: Some(4097)
            })
        );
        assert!(dependencies.corrections.is_empty());
        assert!(dependencies
            .append(&[event(4098, Followup::Correction)])
            .unwrap()
            .revisited
            .is_empty());
    }

    #[test]
    fn repeated_tool_ids_revisit_every_old_result_without_visiting_other_ids() {
        let mut dependencies = ReviewDependencies::default();
        let mut first = event(0, Followup::Blank);
        first.tool_results = vec!["shared".into(), "shared".into()];
        let mut other = event(1, Followup::Blank);
        other.tool_results = vec!["other".into()];
        let mut second = event(2, Followup::Blank);
        second.tool_results = vec!["shared".into()];
        dependencies.append(&[first, other, second]).unwrap();
        for index in 3..=4 {
            let mut use_event = event(index, Followup::Blank);
            use_event.tool_uses = vec!["shared".into()];
            assert_eq!(
                dependencies.append(&[use_event]).unwrap().revisited,
                BTreeSet::from([0, 2])
            );
        }
        assert_eq!(dependencies.results["shared"], [0, 2]);
    }

    #[test]
    fn bare_user_marker_resolves_plan_mode_but_keeps_correction_pending() {
        let mut dependencies = ReviewDependencies::default();
        let mut pending = event(0, Followup::Blank);
        pending.plan_mode = true;
        pending.waits_for_correction = true;
        dependencies.append(&[pending]).unwrap();
        assert_eq!(
            dependencies
                .append(&[event(1, Followup::BareMarker)])
                .unwrap()
                .revisited,
            BTreeSet::from([0])
        );
        assert!(dependencies.plans.is_empty());
        assert_eq!(dependencies.plan_user(0), Some(1));
        assert_eq!(dependencies.first_followups, [0]);
        assert_eq!(dependencies.corrections, [0]);
    }

    #[test]
    fn empty_append_has_no_historical_work_and_invalid_sequence_does_not_mutate() {
        let mut dependencies = ReviewDependencies::default();
        dependencies.append(&[event(0, Followup::Blank)]).unwrap();
        let before = sonic_rs::to_string(&dependencies).unwrap();
        assert_eq!(
            dependencies.append(&[]).unwrap(),
            ReviewChanges {
                start: 1,
                end: 1,
                revisited: BTreeSet::new()
            }
        );
        assert!(dependencies.append(&[event(3, Followup::Blank)]).is_err());
        assert_eq!(sonic_rs::to_string(&dependencies).unwrap(), before);
    }
}
