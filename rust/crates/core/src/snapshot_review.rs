use std::collections::{BTreeSet, HashMap};
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
#[serde(deny_unknown_fields)]
pub struct ReviewDependencies {
    events: usize,
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
        use crate::snapshot_ledger::{map_growth, vec_growth};
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
        let mut bytes = map_growth(&self.results, results)
            + vec_growth(&self.first_followups, 1)
            + vec_growth(&self.corrections, 1)
            + vec_growth(&self.plans, 1)
            + map_growth(&self.followups, 1)
            + map_growth(&self.plan_users, plans)
            + (affected + uses + results + 1) * 4 * size_of::<usize>();
        if let Entry::User(user) = entry {
            for result in user.tool_results() {
                bytes += result.tool_use_id.len() * 2;
                bytes += self
                    .results
                    .get(&result.tool_use_id)
                    .map_or(4usize.max(results) * size_of::<usize>(), |indices| {
                        vec_growth(indices, results)
                    });
            }
        }
        (bytes, affected)
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
        let old_results_capacity = self.results.capacity();
        let old_first_capacity = self.first_followups.capacity();
        let old_corrections_capacity = self.corrections.capacity();
        let old_plans_capacity = self.plans.capacity();
        let old_followups_capacity = self.followups.capacity();
        let old_plan_users_capacity = self.plan_users.capacity();
        let mut revisited = BTreeSet::new();
        for event in events {
            for id in &event.tool_uses {
                if let Some(indices) = self.results.get(id) {
                    revisited.extend(indices.iter().copied().filter(|index| *index < start));
                }
            }
            if event.followup != Followup::Blank {
                for index in self.plans.drain(..) {
                    self.plan_users.insert(index, event.index);
                    if index < start {
                        revisited.insert(index);
                    }
                }
            }
            match event.followup {
                Followup::Structural => {
                    for index in self.first_followups.drain(..) {
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
                    for index in self.corrections.drain(..) {
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
                    self.first_followups.clear();
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
        }
        self.events += events.len();
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
