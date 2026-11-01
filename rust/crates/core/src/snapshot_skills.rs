use crate::snapshot_prepared::{CodexSkillProof, PreparedEvidence, SkillEvidence};
use crate::snapshot_skill_framing::SkillFraming;

#[derive(Clone, Copy)]
enum SkillPhase {
    Origin,
    Prefix,
    Scan,
    Seal,
}

struct SkillAdmission {
    counter: Arc<AtomicUsize>,
}

impl Drop for SkillAdmission {
    fn drop(&mut self) {
        assert!(
            self.counter.fetch_sub(1, Ordering::AcqRel) != 0,
            "Skills framing admission underflow"
        );
    }
}

struct PendingSkillSource {
    path: PathBuf,
    stamp: SourceStamp,
    key: PreparedDiskKey,
    file: File,
    offset: u64,
    phase: SkillPhase,
    provider: Option<Provider>,
    framing: SkillFraming,
    origin: Vec<u8>,
    prefix: Vec<u8>,
    seal: Vec<u8>,
    previous: Option<(SourceStamp, CodexSkillProof)>,
    _admission: SkillAdmission,
}

impl PendingSkillSource {
    fn accounted_bytes(&self) -> usize {
        size_of::<Self>()
            + self.path.capacity()
            + self.framing.accounted_bytes()
            + self.origin.capacity()
            + self.prefix.capacity()
            + self.seal.capacity()
    }
}

enum SkillStep<'a> {
    Pending,
    Native,
    Ready(PreparedSourceOutcome<'a>),
}

impl NativeStore {
    #[cfg(test)]
    fn audit_skill_source_bytes(work: &PendingSkillSource) -> usize {
        let PendingSkillSource {
            path,
            stamp: _,
            key: _,
            file: _,
            offset: _,
            phase: _,
            provider: _,
            framing,
            origin,
            prefix,
            seal,
            previous: _,
            _admission: _,
        } = work;
        size_of::<PendingSkillSource>()
            + path.capacity()
            + framing.audit_bytes()
            + origin.capacity()
            + prefix.capacity()
            + seal.capacity()
    }

    fn admit_skill_source(&self, context: &Value) -> Result<SkillAdmission, SnapshotError> {
        let cap = if Self::foreground_admission(context)? {
            self.config.loads
        } else {
            self.config.loads.saturating_sub(self.config.hook_loads)
        };
        let state = self.lock_state();
        if state.skill_admissions.load(Ordering::Acquire) >= cap {
            return Err(SnapshotError::new(
                Status::RetainedLimit,
                "Skills framing admission exhausted",
            ));
        }
        state.skill_admissions.fetch_add(1, Ordering::AcqRel);
        Ok(SkillAdmission {
            counter: Arc::clone(&state.skill_admissions),
        })
    }

    fn open_skill_source(
        &self,
        canonical: &Path,
        stamp: SourceStamp,
        context: &Value,
        deadline: u64,
        cancel: &Cancellation,
        usage: &mut [u64; 20],
    ) -> Result<File, SnapshotError> {
        self.registry(context)?;
        cancel.check(deadline)?;
        self.authority(context, Some(canonical))?;
        if stamp.size > self.config.source as u64 {
            return Err(SnapshotError::new(
                Status::SourceLimit,
                "source exceeds owner bound",
            ));
        }
        let file = File::open(canonical).map_err(io_error)?;
        usage[0] += 1;
        let metadata = file.metadata().map_err(io_error)?;
        if !metadata.is_file() {
            return Err(invalid("source must be a regular file"));
        }
        if SourceStamp::of(&metadata) != stamp
            || SourceStamp::of(&std::fs::metadata(canonical).map_err(io_error)?) != stamp
        {
            return Err(SnapshotError::new(
                Status::Changed,
                "skill source changed during open",
            ));
        }
        Ok(file)
    }

    fn start_skill_source(
        &self,
        canonical: &Path,
        stamp: SourceStamp,
        key: PreparedDiskKey,
        file: File,
        framing_admission: SkillAdmission,
        context: &Value,
        remaining: &WorkLimits,
        cancel: &Cancellation,
    ) -> Result<PreparedSourceOutcome<'_>, SnapshotError> {
        cancel.check(remaining.deadline_unix_ms)?;
        let reservation = self.reserve_projection(
            context,
            size_of::<PendingSkillSource>() + canonical.as_os_str().len() + 256,
        )?;
        let classifier = json!({"id":"native","version":"1"});
        let registry_generation = str_field(context, "registry_generation")?;
        let admission = str_field(context, "admission")?;
        let (current_codex, previous) = {
            let mut state = self.lock_state();
            let current = state
                .latest_source(&stamp.identity)
                .is_some_and(|snapshot| {
                    snapshot.stamp == stamp && snapshot.provider == Provider::Codex
                });
            let prior = state
                .identity_facts(stamp.identity)
                .filter(|(_, cached)| {
                    cached.stamp.size < stamp.size
                        && cached.stamp.identity.window_base == 0
                        && cached.registry_generation == registry_generation
                        && cached.admission == admission
                        && cached.authority == context["authority"]
                        && cached.classifier == classifier
                })
                .filter_map(|(_, cached)| {
                    let proof = match &cached.facts.evidence {
                        PreparedEvidence::Skills {
                            evidence: SkillEvidence::CodexFramed { proof },
                        } if proof.first_line_committed
                            && proof.validates(cached.stamp.size, self.config.entry) =>
                        {
                            CodexSkillProof {
                                covered_bytes: proof.covered_bytes,
                                line_bound: proof.line_bound,
                                tail_bytes: proof.tail_bytes,
                                first_line_committed: true,
                                fence_seal: proof.fence_seal,
                            }
                        }
                        PreparedEvidence::Index { .. } => {
                            let append = cached.append.as_ref()?;
                            if append.stamp != cached.stamp
                                || append.stamp.identity.window_base != 0
                            {
                                return None;
                            }
                            CodexSkillProof {
                                covered_bytes: append.stamp.size,
                                line_bound: self.config.entry.min(append.stamp.size as usize),
                                tail_bytes: 0,
                                first_line_committed: true,
                                fence_seal: Sha256::digest(&append.fence).into(),
                            }
                        }
                        _ => return None,
                    };
                    Some((cached.stamp, proof))
                })
                .max_by_key(|(stamp, _)| stamp.size);
            (current, prior)
        };
        if current_codex {
            let facts = Arc::new(crate::snapshot_prepared::PreparedFacts::skills(
                SkillEvidence::Parsed { skills: Vec::new() },
            ));
            return self.publish_prepared_evidence(
                stamp,
                reservation,
                facts,
                None,
                context,
                FactsNeed::Skills,
            );
        }
        let work = Box::new(PendingSkillSource {
            path: canonical.to_owned(),
            stamp,
            key,
            file,
            offset: 0,
            phase: SkillPhase::Origin,
            provider: None,
            framing: SkillFraming::new(),
            origin: Vec::new(),
            prefix: Vec::new(),
            seal: Vec::new(),
            previous,
            _admission: framing_admission,
        });
        Ok(PreparedSourceOutcome::SkillPending { work, reservation })
    }

    fn check_skill_source(
        &self,
        work: &PendingSkillSource,
        context: &Value,
    ) -> Result<(), SnapshotError> {
        self.authority(context, Some(&work.path))?;
        if SourceStamp::of(&work.file.metadata().map_err(io_error)?) != work.stamp
            || SourceStamp::of(&std::fs::metadata(&work.path).map_err(io_error)?) != work.stamp
        {
            return Err(SnapshotError::new(
                Status::Changed,
                "skill source changed during preparation",
            ));
        }
        Ok(())
    }

    fn read_skill_range(
        &self,
        file: &mut File,
        stamp: SourceStamp,
        start: u64,
        length: usize,
        bytes: &mut Vec<u8>,
        context: &Value,
        remaining: &mut WorkLimits,
        reservation: &mut ProjectionReservation<'_>,
        usage: &mut [u64; 20],
    ) -> Result<bool, SnapshotError> {
        let count = (length - bytes.len())
            .min(self.config.read_step)
            .min(remaining.max_source_read_bytes);
        if count == 0 && bytes.len() < length {
            return Err(source_read_limit());
        }
        self.extend_projection_reservation(reservation, context, vec_growth(bytes, count))?;
        file.seek(SeekFrom::Start(start + bytes.len() as u64))
            .map_err(io_error)?;
        let old = bytes.len();
        bytes.resize(old + count, 0);
        if count != 0 {
            self.before_source_read();
            file.read_exact(&mut bytes[old..]).map_err(io_error)?;
            usage[1] += count as u64;
            remaining.max_source_read_bytes -= count;
            #[cfg(test)]
            self.observe_hydration(HydrationObservation::Read {
                stamp,
                bytes: count as u64,
            });
        }
        Ok(bytes.len() == length)
    }

    fn classify_skill_source(
        &self,
        work: &mut PendingSkillSource,
        context: &Value,
        remaining: &mut WorkLimits,
        usage: &mut [u64; 20],
    ) -> Result<Option<Provider>, SnapshotError> {
        let Some(line) = work.framing.provider_line() else {
            return Ok(None);
        };
        if line.len() > remaining.max_read_bytes {
            return Err(SnapshotError::new(
                Status::Incomplete,
                "provider classification read budget exhausted",
            ));
        }
        let overflow = || {
            SnapshotError::new(
                Status::RetainedLimit,
                "provider classification bound overflows",
            )
        };
        let classification = dom_parse_bound(line.len())
            .ok_or_else(overflow)?
            .checked_mul(2)
            .and_then(|bytes| bytes.checked_add(line.len().checked_mul(2)?))
            .ok_or_else(overflow)?;
        let _decoded = self.reserve_projection(context, classification)?;
        let provider = sniff_provider(line);
        usage[2] += line.len() as u64;
        remaining.max_read_bytes -= line.len();
        work.provider = Some(provider);
        work.framing.classified();
        Ok(Some(provider))
    }

    fn advance_skill_source(
        &self,
        work: &mut PendingSkillSource,
        context: &Value,
        remaining: &mut WorkLimits,
        reservation: &mut ProjectionReservation<'_>,
        cancel: &Cancellation,
        usage: &mut [u64; 20],
    ) -> Result<SkillStep<'_>, SnapshotError> {
        let source_budget = remaining.max_source_read_bytes;
        let before = usage[1];
        let mut step = *remaining;
        step.max_source_read_bytes = source_budget.min(self.config.read_step);
        let result = (|| loop {
            cancel.check(step.deadline_unix_ms)?;
            self.check_skill_source(work, context)?;
            if let Some(retained) = self.touch_retained_facts(
                &work.key,
                work.stamp,
                str_field(context, "registry_generation")?,
                str_field(context, "admission")?,
                &context["authority"],
                &json!({"id":"native","version":"1"}),
                FactsNeed::Skills,
            ) {
                usage[7] += 1;
                return Ok(SkillStep::Ready(PreparedSourceOutcome::Ready {
                    stamp: work.stamp,
                    held: HeldFacts::Retained(retained),
                    cached: true,
                }));
            }
            let unread = match work.phase {
                SkillPhase::Origin => {
                    (work.stamp.size.min(64) as usize).saturating_sub(work.origin.len())
                }
                SkillPhase::Prefix => {
                    let (stamp, _) = work.previous.as_ref().expect("previous skill proof");
                    (stamp.size.min(64) as usize).saturating_sub(work.prefix.len())
                }
                SkillPhase::Scan => (work.stamp.size - work.offset) as usize,
                SkillPhase::Seal => {
                    (work.stamp.size.min(64) as usize).saturating_sub(work.seal.len())
                }
            };
            if unread != 0 && step.max_source_read_bytes == 0 {
                return if (usage[1] - before) as usize == source_budget {
                    Err(source_read_limit())
                } else {
                    Ok(SkillStep::Pending)
                };
            }
            match self.advance_skill_source_phase(
                work,
                context,
                &mut step,
                reservation,
                cancel,
                usage,
            )? {
                SkillStep::Pending => {}
                finished => return Ok(finished),
            }
        })();
        remaining.max_source_read_bytes =
            source_budget.saturating_sub((usage[1] - before) as usize);
        remaining.max_read_bytes = step.max_read_bytes;
        result
    }

    fn advance_skill_source_phase(
        &self,
        work: &mut PendingSkillSource,
        context: &Value,
        remaining: &mut WorkLimits,
        reservation: &mut ProjectionReservation<'_>,
        cancel: &Cancellation,
        usage: &mut [u64; 20],
    ) -> Result<SkillStep<'_>, SnapshotError> {
        let fence_bytes = (work.stamp.size as usize).min(64);
        match work.phase {
            SkillPhase::Origin => {
                if self.read_skill_range(
                    &mut work.file,
                    work.stamp,
                    work.stamp.size - fence_bytes as u64,
                    fence_bytes,
                    &mut work.origin,
                    context,
                    remaining,
                    reservation,
                    usage,
                )? {
                    work.phase = if work.previous.is_some() {
                        SkillPhase::Prefix
                    } else {
                        SkillPhase::Scan
                    };
                    work.file.seek(SeekFrom::Start(0)).map_err(io_error)?;
                }
            }
            SkillPhase::Prefix => {
                let (stamp, proof) = work.previous.as_ref().expect("previous skill proof");
                if self.read_skill_range(
                    &mut work.file,
                    work.stamp,
                    stamp.size - stamp.size.min(64),
                    stamp.size.min(64) as usize,
                    &mut work.prefix,
                    context,
                    remaining,
                    reservation,
                    usage,
                )? {
                    if <[u8; 32]>::from(Sha256::digest(&work.prefix)) == proof.fence_seal {
                        self.extend_projection_reservation(
                            reservation,
                            context,
                            work.prefix.len(),
                        )?;
                        work.framing = SkillFraming::after_prefix(
                            proof.line_bound,
                            proof.tail_bytes,
                            work.prefix.clone(),
                        );
                        work.offset = stamp.size;
                        work.provider = Some(Provider::Codex);
                    }
                    work.file
                        .seek(SeekFrom::Start(work.offset))
                        .map_err(io_error)?;
                    work.phase = SkillPhase::Scan;
                }
            }
            SkillPhase::Scan => {
                let count = ((work.stamp.size - work.offset) as usize)
                    .min(self.config.read_step)
                    .min(remaining.max_source_read_bytes)
                    .min(if work.provider.is_none() {
                        64 * 1024
                    } else {
                        usize::MAX
                    });
                if count == 0 && work.offset < work.stamp.size {
                    return Err(source_read_limit());
                }
                let _buffer = self.reserve_projection(context, count)?;
                let mut bytes = vec![0; count];
                if count != 0 {
                    self.before_source_read();
                    work.file.read_exact(&mut bytes).map_err(io_error)?;
                    work.offset += count as u64;
                    usage[1] += count as u64;
                    remaining.max_source_read_bytes -= count;
                    #[cfg(test)]
                    self.observe_hydration(HydrationObservation::Read {
                        stamp: work.stamp,
                        bytes: count as u64,
                    });
                }
                for block in bytes.chunks(64 * 1024) {
                    cancel.check(remaining.deadline_unix_ms)?;
                    for part in block.split_inclusive(|byte| *byte == b'\n') {
                        self.extend_projection_reservation(
                            reservation,
                            context,
                            work.framing.growth(part.len()),
                        )?;
                        work.framing.push(part, self.config.entry).map_err(|_| {
                            SnapshotError::new(
                                Status::EntryLimit,
                                "source entry exceeds owner bound",
                            )
                        })?;
                        if self.classify_skill_source(work, context, remaining, usage)?
                            == Some(Provider::Claude)
                        {
                            return Ok(SkillStep::Native);
                        }
                    }
                }
                if work.offset == work.stamp.size {
                    work.framing.finish();
                    if self.classify_skill_source(work, context, remaining, usage)?
                        == Some(Provider::Claude)
                    {
                        return Ok(SkillStep::Native);
                    }
                }
                if work.offset == work.stamp.size {
                    work.phase = SkillPhase::Seal;
                }
            }
            SkillPhase::Seal => {
                if !self.read_skill_range(
                    &mut work.file,
                    work.stamp,
                    work.stamp.size - fence_bytes as u64,
                    fence_bytes,
                    &mut work.seal,
                    context,
                    remaining,
                    reservation,
                    usage,
                )? {
                    return Ok(SkillStep::Pending);
                }
                self.check_skill_source(work, context)?;
                cancel.check(remaining.deadline_unix_ms)?;
                if work.origin != work.seal || work.framing.fence() != work.origin.as_slice() {
                    return Err(SnapshotError::new(
                        Status::Changed,
                        "skill source fence changed",
                    ));
                }
                let mut facts_reservation = self.reserve_projection(
                    context,
                    arc_bytes::<crate::snapshot_prepared::PreparedFacts>()
                        + size_of::<CodexSkillProof>(),
                )?;
                let facts = Arc::new(crate::snapshot_prepared::PreparedFacts::skills(
                    SkillEvidence::CodexFramed {
                        proof: Box::new(CodexSkillProof {
                            covered_bytes: work.stamp.size,
                            line_bound: work.framing.max_line_bytes(),
                            tail_bytes: work.framing.tail_bytes(),
                            first_line_committed: work.framing.first_line_committed(),
                            fence_seal: Sha256::digest(&work.origin).into(),
                        }),
                    },
                ));
                let growth = facts
                    .accounted_bytes()
                    .saturating_sub(facts_reservation.bytes);
                self.extend_projection_reservation(&mut facts_reservation, context, growth)?;
                return self
                    .publish_prepared_evidence(
                        work.stamp,
                        facts_reservation,
                        facts,
                        None,
                        context,
                        FactsNeed::Skills,
                    )
                    .map(SkillStep::Ready);
            }
        }
        Ok(SkillStep::Pending)
    }
}
