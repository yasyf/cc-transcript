pub struct RegisteredReviewInput {
    pub checkpoint: RegisteredReviewCheckpoint,
    pub mining: crate::mining::ReviewMiningIndex,
    pub facts: crate::snapshot_text::SourceFactsIndex,
    pub activity: ActivityIndex,
    spans: Vec<crate::scan_stream::LineSpan>,
    touched: u64,
}

impl RegisteredReviewInput {
    pub fn accounted_bytes(&self) -> usize {
        size_of::<Self>() + self.checkpoint.accounted_bytes()
            - size_of::<RegisteredReviewCheckpoint>()
            + self.mining.accounted_bytes()
            - size_of::<crate::mining::ReviewMiningIndex>()
            + self.facts.accounted_bytes()
            - size_of::<crate::snapshot_text::SourceFactsIndex>()
            + self.activity.accounted_bytes()
            - size_of::<ActivityIndex>()
            + self.spans.capacity() * size_of::<crate::scan_stream::LineSpan>()
    }

    pub fn event_span(&self, event: usize) -> Option<crate::scan_stream::LineSpan> {
        self.spans.get(event).copied()
    }
}

impl Charge<[u8; 32]> for RegisteredReviewInput {
    fn charge(&self) -> usize {
        self.accounted_bytes() - size_of::<Self>()
    }
}

pub struct RegisteredReviewRead<T> {
    pub value: T,
    pub progress: crate::scan::ScanProgress,
    pub mining_usage: crate::mining::MiningUsage,
    pub complete: bool,
}

impl NativeStore {
    fn review_reserve(
        &self,
        reservation: &mut ProjectionReservation<'_>,
        context: &Value,
        needed: usize,
    ) -> Result<(), SnapshotError> {
        if needed > reservation.bytes {
            self.extend_projection_reservation(reservation, context, needed - reservation.bytes)?;
        }
        Ok(())
    }

    fn review_settle(&self, reservation: &mut ProjectionReservation<'_>, bytes: usize) {
        assert!(
            bytes <= reservation.bytes,
            "review input exceeded admitted storage"
        );
        let released = reservation.bytes - bytes;
        self.lock_state().transient_bytes -= released;
        reservation.bytes = bytes;
    }

    fn take_review_input(
        &self,
        epoch: &[u8; 32],
    ) -> (Option<RegisteredReviewInput>, ProjectionReservation<'_>) {
        let mut state = self.lock_state();
        let input = state.review_inputs.remove(epoch);
        let bytes = input
            .as_ref()
            .map_or(0, RegisteredReviewInput::accounted_bytes);
        if let Some(input) = &input {
            assert!(state
                .cache_order
                .remove(&(input.touched, CacheOwner::Review(*epoch))));
        }
        state.transient_bytes += bytes;
        (input, ProjectionReservation { store: self, bytes })
    }

    fn publish_review_input(
        &self,
        mut input: RegisteredReviewInput,
        context: &Value,
        reservation: &mut ProjectionReservation<'_>,
    ) -> Result<(), SnapshotError> {
        let key = input.checkpoint.epoch;
        let mut state = self.lock_state();
        let growth = state.review_inputs.growth_for(&key) + size_of::<(u64, CacheOwner)>();
        let additional = input.charge() + growth;
        let covered = additional.min(reservation.bytes);
        self.admit_memory(&mut state, context, additional - covered)?;
        state.review_inputs.reserve_for(&key);
        input.touched = state.next_cache_touch();
        assert!(state
            .cache_order
            .insert((input.touched, CacheOwner::Review(key))));
        assert!(state.review_inputs.insert(key, input).is_none());
        state.transient_bytes -= covered;
        reservation.bytes -= covered;
        Ok(())
    }

    fn review_decode(
        &self,
        bytes: &[u8],
        context: &Value,
        reservation: &mut ProjectionReservation<'_>,
        retained: usize,
    ) -> Result<Vec<Entry>, SnapshotError> {
        if bytes.len() > self.config.entry {
            return Err(SnapshotError::new(
                Status::EntryLimit,
                "source entry exceeds owner bound",
            ));
        }
        let overflow = || invalid("review entry allocation bound overflow");
        let dom_bound = dom_parse_bound(bytes.len()).ok_or_else(overflow)?;
        self.review_reserve(
            reservation,
            context,
            retained.checked_add(dom_bound).ok_or_else(overflow)?,
        )?;
        let Some(dom) = crate::parse::parse_line_dom(bytes) else {
            return Ok(Vec::new());
        };
        let typed = crate::parse::retained_entry_bound(&dom).ok_or_else(overflow)?
            + crate::parse::retained_arena_bound(&dom)
            + size_of::<Entry>();
        self.review_reserve(
            reservation,
            context,
            retained
                .checked_add(dom_bound)
                .and_then(|bytes| bytes.checked_add(typed))
                .ok_or_else(overflow)?,
        )?;
        let entry =
            crate::parse::entry_from_dom(dom, &mut crate::parse::Retained::default(), &|_| true)
                .map_err(|error| SnapshotError::new(Status::ParseError, format!("{error:?}")))?;
        Ok(entry.into_iter().collect())
    }

    pub fn with_registered_claude_review<'store, T>(
        &'store self,
        path: &Path,
        context: &Value,
        policy: &Value,
        spec: &crate::mining::CompiledMiningSpec,
        first_user_token: &str,
        limits: WorkLimits,
        cancel: &Cancellation,
        consume: impl FnOnce(
            &RegisteredReviewInput,
            &mut crate::scan_stream::SourceStream<'store>,
            &mut crate::scan::ScanBudget<'store>,
        ) -> crate::mining::MiningResult<T>,
    ) -> crate::mining::MiningResult<RegisteredReviewRead<T>> {
        use crate::scan_stream::{LineSpan, SourceStream};
        cancel.check(limits.deadline_unix_ms)?;
        let canonical = std::fs::canonicalize(path).map_err(io_error)?;
        self.authority(context, Some(&canonical))?;
        let registry = self.registry(context)?;
        let binding = json!({
            "policy":{"id":str_field(policy,"id")?,"version":str_field(policy,"version")?},
            "spec_fingerprint":spec.fingerprint(),"provider":"claude","first_user_token":first_user_token,
        });
        let epoch = self.registered_review_epoch(&canonical, context, &binding)?;
        let _claim = self.claim_fact_read(cancel, limits.deadline_unix_ms)?;
        let mut budget = crate::scan::ScanBudget::with_context(self, limits, context.clone());
        let mut stream = SourceStream::open(&canonical, 0, &mut budget, cancel)?;
        if stream.stamp.size > self.config.source as u64 {
            return Err(
                SnapshotError::new(Status::SourceLimit, "source exceeds owner bound").into(),
            );
        }
        let (mut prior, mut reservation) = self.take_review_input(&epoch);
        let mut unchanged = false;
        let mut start = 0;
        if let Some(input) = &prior {
            match input.checkpoint.change(stream.stamp, &epoch) {
                RegisteredReviewChange::Unchanged => {
                    unchanged = true;
                    start = input.checkpoint.committed;
                }
                RegisteredReviewChange::Append { offset } => {
                    let boundary = LineSpan {
                        offset: input.checkpoint.stamp.size
                            - input.checkpoint.boundary.len() as u64,
                        len: input.checkpoint.boundary.len(),
                        terminated: false,
                    };
                    let observed = stream.read_span(&boundary, &mut budget, cancel)?;
                    if input.checkpoint.boundary_matches(&observed) {
                        start = offset;
                    } else {
                        prior = None;
                        budget.progress.cache_invalidations += 1;
                    }
                }
                RegisteredReviewChange::Invalidated => {
                    prior = None;
                    budget.progress.cache_invalidations += 1;
                }
            }
        }
        let mut input = match prior {
            Some(input) => {
                budget.progress.cache_hits += 1;
                input
            }
            None => {
                self.review_reserve(
                    &mut reservation,
                    context,
                    size_of::<RegisteredReviewInput>()
                        + ActivityIndex::EMPTY_ALLOCATION_BYTES
                        + first_user_token.len() * (size_of::<usize>() + 1),
                )?;
                let mining = crate::mining::ReviewMiningIndex::new(spec)?;
                let facts = crate::snapshot_text::SourceFactsIndex::new(first_user_token);
                let input = RegisteredReviewInput {
                    checkpoint: RegisteredReviewCheckpoint {
                        stamp: stream.stamp,
                        committed: 0,
                        events: 0,
                        boundary: Vec::new(),
                        epoch,
                    },
                    mining,
                    facts,
                    activity: ActivityIndex::default(),
                    spans: Vec::new(),
                    touched: 0,
                };
                self.review_reserve(&mut reservation, context, input.accounted_bytes())?;
                input
            }
        };
        if !unchanged {
            stream.seek(start)?;
            while let Some(line) = stream.next_line(&mut budget, cancel)? {
                if !line.terminated {
                    break;
                }
                let bytes = stream.bytes(&line);
                if !bytes.iter().all(u8::is_ascii_whitespace)
                    && sniff_provider(bytes) == Provider::Codex
                {
                    return Err(invalid("registered Claude review requires a Claude source").into());
                }
                let entries =
                    self.review_decode(bytes, context, &mut reservation, input.accounted_bytes())?;
                for entry in &entries {
                    budget.charge_parse(cancel)?;
                    let charge = entry_charge(entry);
                    let heap = charge.owned_capacity_bytes + charge.opaque_dom_accounted_bytes;
                    let bound = input.mining.append_bound(entry, spec)?
                        + vec_growth(&input.spans, 1)
                        + heap * 6
                        + 4096
                        + input.activity.append_container_reservation_bytes(
                            1,
                            entry.tool_uses().count(),
                            entry.tool_results().count(),
                        );
                    self.review_reserve(
                        &mut reservation,
                        context,
                        input.accounted_bytes() + heap + size_of::<Entry>() + bound,
                    )?;
                    let refs = [entry];
                    input.mining.append(&refs, spec)?;
                    input.activity = crate::toolcall::with_registry(Arc::clone(&registry), || {
                        std::mem::take(&mut input.activity).append_tail(&refs, None)
                    });
                    let mut usage = crate::snapshot_text::TextUsage::default();
                    input
                        .facts
                        .append(refs, &budget.remaining(), cancel, &mut usage)?;
                    budget.charge_projection(usage.read_bytes, usage.events, cancel)?;
                    input.spans.push(line);
                }
                input.checkpoint.committed = line.end();
                drop(entries);
                self.review_settle(&mut reservation, input.accounted_bytes());
            }
            input.checkpoint.events = input.mining.event_count();
            let len = (stream.stamp.size as usize).min(64);
            let boundary = LineSpan {
                offset: stream.stamp.size - len as u64,
                len,
                terminated: false,
            };
            self.review_reserve(&mut reservation, context, input.accounted_bytes() + len)?;
            input.checkpoint.boundary = stream.read_span(&boundary, &mut budget, cancel)?;
            input.checkpoint.stamp = stream.stamp;
        }
        let pending = input.mining.pending_count();
        self.review_reserve(
            &mut reservation,
            context,
            input.accounted_bytes() + pending * (size_of::<usize>() + size_of::<(usize, Entry)>()),
        )?;
        let required = input.mining.required_payloads(spec)?;
        let mut payloads = Vec::with_capacity(required.len());
        let mut payload_bytes = required.capacity() * size_of::<usize>()
            + payloads.capacity() * size_of::<(usize, Entry)>();
        for event in required {
            let span = input
                .event_span(event)
                .expect("registered review event span");
            let bytes = stream.read_span(&span, &mut budget, cancel)?;
            let entries = self.review_decode(
                &bytes,
                context,
                &mut reservation,
                input.accounted_bytes() + payload_bytes + bytes.capacity(),
            )?;
            budget.charge_parse(cancel)?;
            let mut entries = entries.into_iter();
            let entry = entries.next().expect("recorded Claude review event");
            assert!(
                entries.next().is_none(),
                "one Claude event per recorded span"
            );
            let charge = entry_charge(&entry);
            payload_bytes += charge.owned_capacity_bytes
                + charge.opaque_dom_accounted_bytes
                + dom_parse_bound(span.len)
                    .ok_or_else(|| invalid("review payload allocation bound overflow"))?;
            payloads.push((event, entry));
        }
        self.review_reserve(
            &mut reservation,
            context,
            input.accounted_bytes()
                + payload_bytes
                + usize::from(pending != 0) * limits.max_output_bytes.saturating_mul(3)
                + pending * size_of::<(usize, &Entry)>(),
        )?;
        let overrides = payloads
            .iter()
            .map(|(event, entry)| (*event, entry))
            .collect();
        let mut mining_usage = crate::mining::MiningUsage::default();
        input.mining.process(
            spec,
            &overrides,
            budget.remaining(),
            cancel,
            &mut mining_usage,
        )?;
        budget.charge_projection(mining_usage.input_bytes, mining_usage.events, cancel)?;
        drop(overrides);
        drop(payloads);
        self.review_settle(&mut reservation, input.accounted_bytes());
        let value = consume(&input, &mut stream, &mut budget)?;
        if stream.verify()?.is_some() {
            return Err(SnapshotError::new(
                Status::Changed,
                "registered review source changed during preparation",
            )
            .into());
        }
        let complete = input.checkpoint.committed == input.checkpoint.stamp.size;
        self.authority(context, Some(&canonical))?;
        self.registry(context)?;
        self.publish_review_input(input, context, &mut reservation)?;
        Ok(RegisteredReviewRead {
            value,
            progress: budget.progress,
            mining_usage,
            complete,
        })
    }
}
