#[cfg(test)]
thread_local! {
    static INTERACTION_INDEX_VISITS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[derive(Clone, Copy)]
struct InteractionWinner {
    first: usize,
    event: usize,
    ordinal: usize,
}

struct InteractionIndex {
    winners: HashMap<Arc<str>, InteractionWinner>,
    names: HashMap<Arc<str>, Vec<Arc<str>>>,
    messages: Vec<usize>,
    accounted_bytes: usize,
}

fn interaction_key(id: &str, timestamp: chrono::DateTime<chrono::FixedOffset>) -> Cow<'_, str> {
    if !id.is_empty() {
        return Cow::Borrowed(id);
    }
    let mut key = String::with_capacity(64);
    std::fmt::Write::write_fmt(&mut key, format_args!("{}", timestamp.format("%Y-%m-%d %H:%M:%S"))).expect("string formatting");
    let micros = timestamp.timestamp_subsec_micros();
    if micros != 0 {
        std::fmt::Write::write_fmt(&mut key, format_args!(".{micros:06}")).expect("string formatting");
    }
    let offset = timestamp.offset().local_minus_utc();
    let absolute = offset.unsigned_abs();
    std::fmt::Write::write_fmt(&mut key, format_args!("{}{:02}:{:02}", if offset < 0 { '-' } else { '+' }, absolute / 3600, absolute / 60 % 60)).expect("string formatting");
    if absolute % 60 != 0 {
        std::fmt::Write::write_fmt(&mut key, format_args!(":{:02}", absolute % 60)).expect("string formatting");
    }
    Cow::Owned(key)
}

impl InteractionIndex {
    fn construction_bound(snapshot: &TranscriptSnapshot, cancel: &Cancellation, bounds: WorkLimits) -> Result<(usize, usize, usize), SnapshotError> {
        let deadline = bounds.deadline_unix_ms;
        let event_work = snapshot.event_count * (3 * size_of::<usize>() + size_of::<bool>());
        let exhausted = || SnapshotError::new(Status::Incomplete, "interaction index work budget exhausted");
        if event_work > bounds.max_read_bytes || snapshot.event_count > bounds.max_events {
            return Err(exhausted());
        }
        let mut count = 0usize;
        let mut strings = 0usize;
        let mut metadata = 0usize;
        for (_, _, id, name) in snapshot.activity.call_positions() {
            cancel.check(deadline)?;
            #[cfg(test)]
            INTERACTION_INDEX_VISITS.with(|visits| visits.set(visits.get() + 1));
            count += 1;
            metadata += id.len() + name.len() + size_of::<InteractionWinner>();
            if metadata * 2 > bounds.max_read_bytes - event_work || count * 2 > bounds.max_events - snapshot.event_count {
                return Err(exhausted());
            }
            strings += arc_slice_bytes::<u8>(id.len().max(64)) + arc_slice_bytes::<u8>(name.len());
        }
        let winners: HashMap<Arc<str>, InteractionWinner> = HashMap::new();
        let names: HashMap<Arc<str>, Vec<Arc<str>>> = HashMap::new();
        let seen: HashSet<(Arc<str>, Arc<str>)> = HashSet::new();
        let bytes = arc_bytes::<Self>() + map_growth(&winners, count) + map_growth(&names, count)
            + set_growth(&seen, count) + strings + count * 4 * size_of::<Arc<str>>()
            + crate::parse::pushed_capacity(snapshot.event_count) * size_of::<usize>() + 64;
        Ok((bytes, metadata * 2 + event_work, count * 2 + snapshot.event_count))
    }

    fn new(snapshot: &TranscriptSnapshot, cancel: &Cancellation, deadline: u64) -> Result<Self, SnapshotError> {
        let mut winners: HashMap<Arc<str>, InteractionWinner> = HashMap::new();
        let mut names: HashMap<Arc<str>, Vec<Arc<str>>> = HashMap::new();
        let mut seen: HashSet<(Arc<str>, Arc<str>)> = HashSet::new();
        for (first, (event, ordinal, id, name)) in snapshot.activity.call_positions().enumerate() {
            cancel.check(deadline)?;
            #[cfg(test)]
            INTERACTION_INDEX_VISITS.with(|visits| visits.set(visits.get() + 1));
            let timestamp = snapshot.entry(event).meta().expect("indexed call event has metadata").timestamp;
            let key = interaction_key(id, timestamp);
            let key: Arc<str> = winners.get_key_value(key.as_ref())
                .map_or_else(|| Arc::from(key.as_ref()), |(key, _)| Arc::clone(key));
            winners.entry(Arc::clone(&key))
                .and_modify(|winner| { winner.event = event; winner.ordinal = ordinal; })
                .or_insert(InteractionWinner { first, event, ordinal });
            let name: Arc<str> = names.get_key_value(name)
                .map_or_else(|| Arc::from(name), |(name, _)| Arc::clone(name));
            if seen.insert((Arc::clone(&name), Arc::clone(&key))) {
                names.entry(name).or_default().push(key);
            }
        }
        let mut messages = Vec::new();
        for (event, entry) in snapshot.chunks.iter().flat_map(|chunk| chunk.entries.iter()).enumerate() {
            cancel.check(deadline)?;
            #[cfg(test)]
            INTERACTION_INDEX_VISITS.with(|visits| visits.set(visits.get() + 1));
            let candidate = match entry {
                Entry::User(user) => user.meta.is_meta,
                Entry::Attachment(attachment) => matches!(&attachment.detail,
                    crate::types::AttachmentDetail::QueuedCommand(queued) if queued.origin.as_deref() == Some("channel")),
                _ => false,
            };
            if candidate && snapshot.activity.turn_of_event(event)
                .and_then(|turn| snapshot.activity.turn_bounds(turn)).is_some_and(|range| range.contains(&event)) {
                messages.push(event);
            }
        }
        let accounted_bytes = arc_bytes::<Self>() + messages.capacity() * size_of::<usize>()
            + winners.capacity() * size_of::<(Arc<str>, InteractionWinner)>()
            + winners.keys().map(|key| arc_slice_bytes::<u8>(key.len())).sum::<usize>()
            + names.capacity() * size_of::<(Arc<str>, Vec<Arc<str>>)>()
            + names.iter().map(|(name, keys)| arc_slice_bytes::<u8>(name.len()) + keys.capacity() * size_of::<Arc<str>>()).sum::<usize>();
        Ok(Self { winners, names, messages, accounted_bytes })
    }
}

struct InteractionScope<'s> {
    store: &'s NativeStore,
    snapshot: Arc<TranscriptSnapshot>,
    index: Arc<InteractionIndex>,
    read_bytes: usize,
    events: usize,
}

impl NativeStore {
    fn indexed_interactions<'s>(
        &'s self, snapshot: Arc<TranscriptSnapshot>, context: &Value,
        cancel: &Cancellation, bounds: WorkLimits,
    ) -> Result<InteractionScope<'s>, SnapshotError> {
        let current_path = std::fs::canonicalize(&snapshot.canonical_path).map_err(io_error)?;
        self.authority(context, Some(&current_path))?;
        let deadline = bounds.deadline_unix_ms;
        let _claim = self.claim_fact_read(cancel, deadline)?;
        let id = snapshot_key(&snapshot);
        let cached = self.lock_state().generations.get(&id)
            .and_then(|generation| generation.interactions.as_ref().map(Arc::clone));
        if let Some(index) = cached {
            return Ok(InteractionScope { store: self, snapshot, index, read_bytes: 0, events: 0 });
        }
        cancel.check(deadline)?;
        let (bytes, read_bytes, events) = InteractionIndex::construction_bound(&snapshot, cancel, bounds)?;
        if read_bytes > bounds.max_read_bytes || events > bounds.max_events {
            return Err(SnapshotError::new(Status::Incomplete, "interaction index work budget exhausted"));
        }
        let mut reservation = self.reserve_projection(context, bytes)?;
        let index = Arc::new(InteractionIndex::new(&snapshot, cancel, deadline)?);
        assert!(index.accounted_bytes <= reservation.bytes, "interaction index exceeded its admitted bound");
        cancel.check(deadline)?;
        let mut state = self.lock_state();
        let current_path = std::fs::canonicalize(&snapshot.canonical_path).map_err(io_error)?;
        self.authority(context, Some(&current_path))?;
        state.transient_bytes -= index.accounted_bytes;
        reservation.bytes -= index.accounted_bytes;
        state.generations.get_mut(&id).expect("pinned generation").interactions = Some(Arc::clone(&index));
        Ok(InteractionScope { store: self, snapshot, index, read_bytes, events })
    }
}

#[cfg(test)]
impl InteractionIndex {
    fn audited_bytes(&self) -> usize {
        let Self { winners, names, messages, accounted_bytes: _ } = self;
        arc_mirror::<Self>() + messages.capacity() * size_of::<usize>()
            + winners.capacity() * size_of::<(Arc<str>, InteractionWinner)>()
            + winners.keys().map(|key| arc_slice_mirror::<u8>(key.len())).sum::<usize>()
            + names.capacity() * size_of::<(Arc<str>, Vec<Arc<str>>)>()
            + names.iter().map(|(name, keys)| arc_slice_mirror::<u8>(name.len()) + keys.capacity() * size_of::<Arc<str>>()).sum::<usize>()
    }
}

impl InteractionScope<'_> {
    fn keys<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a str> {
        self.index.names.iter()
            .filter(move |(actual, _)| crate::toolcall::tool_name_matches(actual, name))
            .flat_map(|(_, keys)| keys.iter().map(|key| key.as_ref()))
    }

    fn winner(&self, key: &str) -> Option<InteractionWinner> {
        self.index.winners.get(key).copied()
    }

    fn call<'a>(
        &'a self, winner: InteractionWinner, reservation: &mut ProjectionReservation<'_>, context: &Value,
    ) -> Result<crate::activity::ToolUse<'a>, SnapshotError> {
        let bytes = self.snapshot.activity.projected_call_at_bytes(winner.event, winner.ordinal)
            .expect("interaction winner belongs to its snapshot");
        self.store.extend_projection_reservation(reservation, context, bytes)?;
        Ok(self.snapshot.activity.project_call_at(winner.event, winner.ordinal, |event| self.snapshot.entry(event))
            .expect("interaction winner belongs to its snapshot"))
    }
}
