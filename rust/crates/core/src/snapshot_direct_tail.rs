struct DirectTailSelection {
    stamp: SourceStamp,
    count: usize,
    budget: usize,
    read: usize,
    exhausted: bool,
    lines: Vec<(u64, Vec<Entry>)>,
    charge: MemoryCharge,
}

fn direct_tail_anchor(tail: &Arc<DirectTailSelection>) -> Anchor {
    Anchor::entries(Arc::as_ptr(tail) as usize, tail.charge)
}

impl NativeStore {
    fn direct_tail_source(&self, stamp: SourceStamp, context: &Value) -> Option<Arc<TranscriptSnapshot>> {
        let registry = context.get("registry_generation")?.as_str()?;
        let mut state = self.lock_state();
        let generation = state.source_spines.get(&stamp.identity.file())?.iter().find_map(|view| {
            state.source_view_checks.tick(1);
            let record = state.generations.get(&view.generation).expect("source generation");
            let snapshot = &record.cached.as_ref().expect("cached source").snapshot;
            (record.registry_generation == registry && stamp.viewed_as(snapshot.stamp) == snapshot.stamp)
                .then_some(view.generation)
        })?;
        Some(state.cached_snapshot(generation))
    }

    fn retain_direct_tail(
        &self, source: Arc<TranscriptSnapshot>, stamp: SourceStamp, count: usize,
        budget: usize, read: usize, exhausted: bool, lines: Vec<(u64, Vec<Entry>)>,
        context: &Value, cancel: &Cancellation, deadline: u64,
        reservation: &mut ProjectionReservation<'_>,
    ) {
        let mut state = self.lock_state();
        let key = snapshot_key(&source);
        if state.generations.get(&key).expect("pinned source generation").direct_tail.is_some()
            || cancel.check(deadline).is_err()
        {
            return;
        }
        let mut charge = MemoryCharge {
            owned_capacity_bytes: arc_bytes::<DirectTailSelection>()
                + lines.capacity() * size_of::<(u64, Vec<Entry>)>(),
            opaque_dom_accounted_bytes: 0,
        };
        for (_, entries) in &lines {
            charge.owned_capacity_bytes += entries.capacity() * size_of::<Entry>();
            for entry in entries {
                charge += entry_charge(entry);
            }
        }
        let bytes = charge.owned_capacity_bytes + charge.opaque_dom_accounted_bytes;
        let growth = bytes.saturating_sub(reservation.bytes);
        if growth > 0 && self.extend_projection_reservation_in(&mut state, reservation, context, growth).is_err() {
            return;
        }
        let tail = Arc::new(DirectTailSelection { stamp, count, budget, read, exhausted, lines, charge });
        let anchor = direct_tail_anchor(&tail);
        let covered = state.ledger.shared.unowned_bytes([anchor]).min(reservation.bytes);
        let fresh = state.ledger.shared.admission([anchor]) - covered;
        if self.admit_memory(&mut state, context, fresh).is_err() || cancel.check(deadline).is_err() {
            return;
        }
        state.ledger.shared.reserve([anchor]);
        state.ledger.shared.acquire(anchor);
        state.transient_bytes -= covered;
        reservation.bytes -= covered;
        state.generations.get_mut(&key).expect("pinned source generation").direct_tail = Some(tail);
    }

    #[cfg(test)]
    fn audit_direct_tail_bytes(tail: &DirectTailSelection) -> usize {
        let DirectTailSelection { stamp: _, count: _, budget: _, read: _, exhausted: _, lines, charge: _ } = tail;
        arc_mirror::<DirectTailSelection>()
            + lines.capacity() * size_of::<(u64, Vec<Entry>)>()
            + lines.iter().map(|(_, entries)| {
                entries.capacity() * size_of::<Entry>()
                    + entries.iter().map(|entry| {
                        let charge = entry_charge(entry);
                        charge.owned_capacity_bytes + charge.opaque_dom_accounted_bytes
                    }).sum::<usize>()
            }).sum::<usize>()
    }
}
