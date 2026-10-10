impl NativeStore {
    fn warm_root(
        &self,
        request: &Value,
        context: &Value,
        cancel: &Cancellation,
        usage: &mut [u64; 20],
    ) -> Result<(Value, Option<String>, Option<String>), SnapshotError> {
        let bounds = limits(request)?;
        if context["work_class"].as_str() != Some("background")
            || str_field(context, "admission")? != "hook"
        {
            return Err(invalid("root warming requires background hook admission"));
        }
        let classifier = request
            .get("classifier")
            .ok_or_else(|| invalid("missing classifier"))?;
        cancel.check(bounds.deadline_unix_ms)?;
        let path = std::fs::canonicalize(str_field(request, "path")?).map_err(io_error)?;
        self.authority(context, Some(&path))?;
        let metadata = std::fs::metadata(&path).map_err(io_error)?;
        if !metadata.is_file() {
            return Err(invalid("root source must be a regular file"));
        }
        let current = SourceStamp::of(&metadata).windowed(tail_bytes(request)?);
        if current.window_bytes() > self.config.source as u64 {
            return Err(SnapshotError::new(
                Status::SourceLimit,
                "root source exceeds owner bound",
            ));
        }
        let pinned = {
            let mut state = self.lock_state();
            let superseded: Vec<_> = state
                .prepared_loads
                .keys()
                .filter(|identity| {
                    current.identity.window_base > 0
                        && identity.window_base > 0
                        && identity.file() == current.identity.file()
                        && **identity != current.identity
                })
                .copied()
                .collect();
            for identity in superseded {
                state.prepared_loads.remove(&identity);
                if state
                    .loads
                    .get(&identity)
                    .is_some_and(|slot| Arc::strong_count(slot) == 1)
                {
                    state.remove_load(&identity);
                }
            }
            if let Some((slot, touched)) = state.prepared_loads.get_mut(&current.identity) {
                if Self::matches_prefix(current, slot.stamp)
                    && slot.path == path
                    && slot.registry_generation == str_field(context, "registry_generation")?
                {
                    slot.deadline.store(
                        now_ms().saturating_add(self.config.preparation),
                        Ordering::Release,
                    );
                    *touched = now_ms();
                    Some(Arc::clone(slot))
                } else {
                    state.prepared_loads.remove(&current.identity);
                    state.remove_load(&current.identity);
                    None
                }
            } else {
                None
            }
        };
        let stamp = pinned.as_ref().map_or(current, |slot| slot.stamp);
        let outcome = if let Some(slot) = pinned {
            self.resume_warm_root(
                slot,
                classifier,
                context,
                bounds,
                tail_bytes(request)?.is_some(),
                cancel,
                usage,
            )
        } else {
            self.acquire(request, context, cancel, usage)
        };
        let pending = outcome.as_ref().ok().and_then(|(data, token, _)| {
            token
                .clone()
                .map(|token| (token, data["reservation"]["load_id"].as_str().map(str::to_owned)))
        });
        if let Some((token, _)) = &pending {
            self.lock_state()
                .waiters
                .remove(token);
        }
        let (slot, stamp) = {
            let mut state = self.lock_state();
            let slot = match pending.as_ref().and_then(|(_, load_id)| load_id.as_deref()) {
                Some(load_id) => state.loads.values().find(|slot| slot.id == load_id).cloned(),
                None => state.loads.get(&stamp.identity).cloned(),
            };
            let stamp = slot.as_ref().map_or(stamp, |slot| slot.stamp);
            if pending.is_some()
                || outcome.as_ref().err().is_some_and(|error| {
                    matches!(error.status, Status::Deadline | Status::Cancelled)
                })
            {
                if let Some(slot) = &slot {
                    let growth = state.prepared_load_growth(&stamp.identity);
                    if self.admit_memory(&mut state, context, growth).is_ok() {
                        state.insert_prepared_load(stamp.identity, Arc::clone(slot), now_ms());
                    }
                }
            } else {
                state.prepared_loads.remove(&stamp.identity);
            }
            (slot, stamp)
        };
        let source_offset = slot
            .as_ref()
            .map_or(0, |slot| slot.work.lock().expect("root load").offset);
        let progress = |complete: bool| {
            (
                json!({"kind":"warmed_root","owner_epoch":self.owner_epoch,"source_revision":stamp.revision(),"source_offset":source_offset,"source_size":stamp.size,"complete":complete,"facts_complete":complete}),
                None,
                None,
            )
        };
        let (data, cursor, _) = match outcome {
            Ok(outcome) => outcome,
            Err(error) if error.status == Status::Deadline && (usage[1] > 0 || usage[3] > 0) => {
                return Ok(progress(false));
            }
            Err(error) => return Err(error),
        };
        if cursor.is_some() {
            return Ok(progress(false));
        }
        let handle = &data["description"]["handle"];
        let lease_id = str_field(handle, "lease_id")?;
        let snapshot = self.pin_scope_for_work(handle, context, bounds.deadline_unix_ms);
        self.lock_state()
            .leases
            .remove(lease_id);
        let snapshot = match snapshot {
            Ok((snapshot, _)) => snapshot,
            Err(error) if error.status == Status::Deadline => return Ok(progress(false)),
            Err(error) => return Err(error),
        };
        if snapshot.stamp != stamp {
            return Err(SnapshotError::new(Status::Changed, "warmed root changed"));
        }
        match self.prepared_root_facts(
            &snapshot,
            classifier,
            context,
            &bounds,
            PreparedCacheReads::Within(
                bounds
                    .max_source_read_bytes
                    .saturating_sub(usage[1] as usize),
            ),
            cancel,
            usage,
        ) {
            Ok(_) => Ok(progress(true)),
            Err(error) if error.status == Status::Deadline => Ok(progress(false)),
            Err(error)
                if error.status == Status::Incomplete
                    && error.reason == "prepared_cache_read_limit" =>
            {
                if usage[1] > 0 {
                    Ok(progress(false))
                } else {
                    Err(prepared_cache_entry_limit())
                }
            }
            Err(error) => Err(error),
        }
    }

    fn resume_warm_root(
        &self,
        slot: Arc<LoadSlot>,
        classifier: &Value,
        context: &Value,
        bounds: WorkLimits,
        windowed: bool,
        cancel: &Cancellation,
        usage: &mut [u64; 20],
    ) -> Result<(Value, Option<String>, Option<String>), SnapshotError> {
        let now = now_ms();
        let token = self.token("reservation");
        let deadline = bounds
            .deadline_unix_ms
            .min(slot.deadline.load(Ordering::Acquire));
        let waiter = Waiter {
            claimant: str_field(context, "claimant")?.to_owned(),
            load: slot,
            classifier: classifier.clone(),
            context: context.clone(),
            limits: bounds,
            created: now,
            expires: (now + self.config.ttl).min(deadline),
            deadline,
            used_bytes: 0,
            used_source_bytes: 0,
            used_events: 0,
            stage: None,
            windowed,
            busy: false,
        };
        {
            let mut state = self.lock_state();
            Self::prune(&mut state);
            if state.waiters.len() >= self.lease_cap(context)? {
                return Err(SnapshotError::new(
                    Status::LeaseLimit,
                    "root warming reservation admission exhausted",
                ));
            }
            let additional = charged_bytes(&token, &waiter) + state.waiters.growth_for(&token);
            self.admit_memory(&mut state, context, additional)?;
            state.insert_waiter(token.clone(), waiter.clone());
        }
        self.advance(&token, waiter, None, cancel, usage)
    }
}

#[cfg(test)]
mod root_warm_tests {
    use super::*;

    fn context(claimant: &str) -> Value {
        json!({"claimant":claimant,"admission":"hook","work_class":"background","authority":{"kind":"user","effective_uid":unsafe { libc::geteuid() }.to_string()},"registry_generation":crate::toolcall::ToolRegistrySnapshot::from_specs(HashMap::new()).fingerprint()})
    }

    fn source(bytes: usize) -> (PathBuf, PathBuf) {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let directory = std::env::temp_dir().join(format!(
            "cc-warm-root-{}-{}-{}",
            std::process::id(),
            now_ms(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&directory).unwrap();
        let path = directory.join("root.jsonl");
        let payload = "x".repeat(16 * 1024);
        let mut file = File::create(&path).unwrap();
        let mut written = 0;
        let mut index = 0;
        while written < bytes {
            let event = format!(
                "{{\"type\":\"user\",\"uuid\":\"root-{index}\",\"sessionId\":\"s\",\"timestamp\":\"2026-01-02T03:04:05Z\",\"message\":{{\"content\":\"{payload}\"}}}}\n"
            );
            file.write_all(event.as_bytes()).unwrap();
            written += event.len();
            index += 1;
        }
        (directory, path)
    }

    fn warm_request(path: &Path, read_bytes: usize) -> Value {
        json!({"schema":SCHEMA,"id":"warm-root","operation":"warm_root","path":path.to_string_lossy().as_ref(),"classifier":{"id":"native","version":"1"},"deadline_unix_ms":now_ms()+30_000,"limits":{"max_read_bytes":read_bytes,"max_source_read_bytes":read_bytes,"max_events":100_000,"max_items":256,"max_output_bytes":1024*1024,"max_discovery_entries":1000,"max_sources":100}})
    }

    #[test]
    fn large_claude_source_reserves_only_one_read_step() {
        let (directory, path) = source(1024);
        File::create(&path).unwrap().set_len(16 * 1024 * 1024).unwrap();
        let store = NativeStore::new(&json!({"max_read_bytes_per_step":8*1024*1024,"max_entry_bytes":32*1024*1024,"max_source_bytes":32*1024*1024,"max_retained_bytes":64*1024*1024,"reserved_hook_accounted_bytes":16*1024*1024,"max_leases":16,"reserved_hook_leases":1})).unwrap();
        let request = warm_request(&path, 8 * 1024 * 1024);
        let context = context("step-reservation");
        let first = store.request(&request, &context, &Cancellation::default());
        assert_eq!(first["status"].as_str(), Some("ok"), "{first:?}");
        let second = store.request(&request, &context, &Cancellation::default());
        assert_eq!(second["status"].as_str(), Some("ok"), "{second:?}");
        assert!(second["data"]["source_offset"].as_u64().unwrap() > 0);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn warms_large_root_across_bounded_calls_without_rereading_source() {
        let (directory, path) = source(12 * 1024 * 1024);
        let source_size = std::fs::metadata(&path).unwrap().len();
        let store = NativeStore::new(&json!({"max_read_bytes_per_step":1024*1024,"max_retained_bytes":256*1024*1024,"reserved_hook_accounted_bytes":4096,"max_leases":16,"reserved_hook_leases":1})).unwrap();
        let context = context("root-warm");
        let request = warm_request(&path, 1024 * 1024);
        let mut source_offset = 0;
        let mut read_bytes = 0;
        let mut finished = false;
        for _ in 0..64 {
            let reply = store.request(&request, &context, &Cancellation::default());
            assert_eq!(reply["status"].as_str(), Some("ok"), "{reply:?}");
            let offset = reply["data"]["source_offset"].as_u64().unwrap();
            assert!(offset >= source_offset);
            source_offset = offset;
            let step = reply["usage"]["source_bytes_read"].as_u64().unwrap();
            assert!(step <= 1024 * 1024);
            read_bytes += step;
            if reply["data"]["complete"].as_bool() == Some(true) {
                assert_eq!(reply["data"]["facts_complete"].as_bool(), Some(true));
                finished = true;
                break;
            }
        }
        assert!(finished);
        assert_eq!(source_offset, source_size);
        assert!(read_bytes >= source_size);
        assert!(read_bytes <= source_size + 128);
        let repeat = store.request(&request, &context, &Cancellation::default());
        assert_eq!(repeat["status"].as_str(), Some("ok"), "{repeat:?}");
        assert_eq!(repeat["data"]["complete"].as_bool(), Some(true));
        assert_eq!(repeat["usage"]["source_bytes_read"].as_u64(), Some(0));
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn root_warming_meters_its_fact_cache_read_against_its_read_budget() {
        use std::os::unix::fs::DirBuilderExt;

        let (directory, path) = source(64 * 1024);
        let store = NativeStore::new(&json!({"max_read_bytes_per_step":1024*1024,"max_retained_bytes":256*1024*1024,"reserved_hook_accounted_bytes":4096,"max_leases":16,"reserved_hook_leases":1})).unwrap();
        let context = context("root-budget");
        let warm = |read_bytes: u64| {
            store.request(
                &warm_request(&path, read_bytes as usize),
                &context,
                &Cancellation::default(),
            )
        };
        let outcome = |reply: Value| {
            (
                reply["status"].as_str().map(str::to_owned),
                reply["reason"].as_str().map(str::to_owned),
                reply["data"]["facts_complete"].as_bool(),
                reply["usage"]["prepared_cache_reads"].as_u64(),
                reply["usage"]["prepared_cache_bytes_read"].as_u64(),
                reply["usage"]["source_bytes_read"].as_u64(),
            )
        };
        let complete = |reads: u64, bytes: u64| {
            (
                Some("ok".to_owned()),
                None,
                Some(true),
                Some(reads),
                Some(bytes),
                Some(0),
            )
        };
        let refused = |status: &str, reason: &str| {
            (
                Some(status.to_owned()),
                Some(reason.to_owned()),
                None,
                Some(0),
                Some(0),
                Some(0),
            )
        };
        let entry_limit = || {
            refused(
                "source_limit",
                "prepared facts entry exceeds the warming read budget",
            )
        };
        let mut built = warm(1024 * 1024);
        for _ in 0..16 {
            if built["data"]["complete"].as_bool() == Some(true) {
                break;
            }
            built = warm(1024 * 1024);
        }
        assert_eq!(built["data"]["complete"].as_bool(), Some(true), "{built:?}");
        let stamp = SourceStamp::of(&std::fs::metadata(std::fs::canonicalize(&path).unwrap()).unwrap());
        let key = crate::snapshot_prepared_disk::PreparedDiskKey::new(
            stamp,
            context["registry_generation"].as_str().unwrap(),
            "hook",
            &context["authority"],
            &json!({"id":"native","version":"1"}),
        )
        .unwrap();
        let entry = store.prepared_disk.entry_file(&key);
        let stored = store.prepared_disk.entry_file_len(&key);
        let resident = || store.lock_state().prepared_facts.contains_key(&stamp.identity);
        let evict = || {
            store
                .lock_state()
                .remove_prepared_facts(&stamp.identity)
                .unwrap();
        };
        assert_eq!(outcome(warm(1024 * 1024)), complete(0, 0));

        evict();
        for _ in 0..2 {
            assert_eq!(outcome(warm(stored)), entry_limit());
        }
        assert!(!resident());

        let owner_directory = entry.parent().unwrap();
        let displaced = owner_directory.with_extension("displaced");
        std::fs::rename(owner_directory, &displaced).unwrap();
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(owner_directory)
            .unwrap();
        let foreign = [outcome(warm(stored)), outcome(warm(stored + 1))];
        std::fs::remove_dir(owner_directory).unwrap();
        std::fs::rename(&displaced, owner_directory).unwrap();
        let changed = refused("incomplete", "prepared facts cache directory changed");
        assert_eq!(foreign, [changed.clone(), changed]);
        assert!(!resident());

        assert_eq!(outcome(warm(stored + 1)), complete(1, stored));
        assert!(resident());

        evict();
        std::fs::remove_file(&entry).unwrap();
        for _ in 0..2 {
            assert_eq!(outcome(warm(stored)), entry_limit());
        }
        assert_eq!(
            outcome(warm(stored + 1)),
            refused("incomplete", "prepared root facts revision was evicted")
        );
        assert!(!store.prepared_disk.has_entry(&key).unwrap());
        assert_eq!(outcome(warm(stored)), complete(0, 0));
        assert_eq!(store.prepared_disk.entry_file_len(&key), stored);
        assert!(resident());
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn root_warming_defers_a_fact_read_its_source_read_left_no_budget_for() {
        #[derive(Clone, Copy, Debug, PartialEq)]
        struct Step {
            complete: bool,
            cache_reads: u64,
            cache_bytes: u64,
            source_bytes: u64,
        }

        let (directory, path) = source(64 * 1024);
        let store = NativeStore::new(&json!({"max_read_bytes_per_step":1024*1024,"max_retained_bytes":256*1024*1024,"reserved_hook_accounted_bytes":4096,"max_leases":16,"reserved_hook_leases":1})).unwrap();
        let context = context("root-deferral");
        let drive = |budget: u64| {
            let mut steps: Vec<Step> = Vec::new();
            while steps.last().map_or(true, |step| !step.complete) {
                assert!(steps.len() < 32, "{steps:?}");
                let reply = store.request(
                    &warm_request(&path, budget as usize),
                    &context,
                    &Cancellation::default(),
                );
                assert_eq!(reply["status"].as_str(), Some("ok"), "{reply:?}");
                steps.push(Step {
                    complete: reply["data"]["facts_complete"].as_bool().unwrap(),
                    cache_reads: reply["usage"]["prepared_cache_reads"].as_u64().unwrap(),
                    cache_bytes: reply["usage"]["prepared_cache_bytes_read"]
                        .as_u64()
                        .unwrap(),
                    source_bytes: reply["usage"]["source_bytes_read"].as_u64().unwrap(),
                });
            }
            assert!(
                steps
                    .iter()
                    .all(|step| step.source_bytes + step.cache_bytes <= budget),
                "{steps:?}"
            );
            steps
        };
        drive(1024 * 1024);
        let stamp =
            SourceStamp::of(&std::fs::metadata(std::fs::canonicalize(&path).unwrap()).unwrap());
        let stored = store.prepared_disk.entry_file_len(
            &crate::snapshot_prepared_disk::PreparedDiskKey::new(
                stamp,
                context["registry_generation"].as_str().unwrap(),
                "hook",
                &context["authority"],
                &json!({"id":"native","version":"1"}),
            )
            .unwrap(),
        );
        let forget = || {
            let mut state = store.lock_state();
            state.remove_prepared_facts(&stamp.identity).unwrap();
            state.remove_load(&stamp.identity).unwrap();
            state.remove_latest(&stamp.identity).unwrap();
        };
        let totals = |steps: &[Step]| {
            (
                steps.iter().map(|step| step.cache_reads).sum::<u64>(),
                steps.iter().map(|step| step.source_bytes).sum::<u64>() >= stamp.size,
            )
        };

        forget();
        let calibration = drive(1024 * 1024);
        let fence = calibration.last().unwrap().source_bytes;
        assert!(fence > 0, "{calibration:?}");
        assert_eq!(totals(&calibration), (1, true), "{calibration:?}");
        assert_eq!(
            calibration.last(),
            Some(&Step {
                complete: true,
                cache_reads: 1,
                cache_bytes: stored,
                source_bytes: fence,
            })
        );

        forget();
        let deferred = drive(fence + stored);
        assert_eq!(totals(&deferred), (1, true), "{deferred:?}");
        assert_eq!(
            deferred[deferred.len() - 2..],
            [
                Step {
                    complete: false,
                    cache_reads: 0,
                    cache_bytes: 0,
                    source_bytes: fence,
                },
                Step {
                    complete: true,
                    cache_reads: 1,
                    cache_bytes: stored,
                    source_bytes: 0,
                },
            ],
            "{deferred:?}"
        );

        forget();
        let admitted = drive(fence + stored + 1);
        assert_eq!(totals(&admitted), (1, true), "{admitted:?}");
        assert_eq!(
            admitted.last(),
            Some(&Step {
                complete: true,
                cache_reads: 1,
                cache_bytes: stored,
                source_bytes: fence,
            }),
            "{admitted:?}"
        );
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn warms_a_trailing_window_without_reading_the_whole_root() {
        let (directory, path) = source(4 * 1024 * 1024);
        let bytes = std::fs::read(&path).unwrap();
        let size = bytes.len() as u64;
        let base = (size - 1024 * 1024) / (512 * 1024) * (512 * 1024);
        let start = bytes[..base as usize]
            .iter()
            .rposition(|byte| *byte == b'\n')
            .map_or(0, |at| at as u64 + 1);
        assert!(start < base);
        let store = NativeStore::new(&json!({"max_read_bytes_per_step":256*1024,"max_retained_bytes":128*1024*1024,"reserved_hook_accounted_bytes":4096,"max_leases":16,"reserved_hook_leases":1})).unwrap();
        let context = context("window-warm");
        let mut request = warm_request(&path, 256 * 1024);
        request.insert("tail_bytes", json!(1024 * 1024));
        let mut total_read = 0;
        let mut finished = false;
        for _ in 0..24 {
            let reply = store.request(&request, &context, &Cancellation::default());
            assert_eq!(reply["status"].as_str(), Some("ok"), "{reply:?}");
            total_read += reply["usage"]["source_bytes_read"].as_u64().unwrap();
            if reply["data"]["complete"].as_bool() == Some(true) {
                assert_eq!(reply["data"]["source_offset"].as_u64(), Some(size));
                finished = true;
                break;
            }
        }
        assert!(finished);
        assert!(total_read >= size - start + 128, "{total_read}");
        assert!(total_read <= size - start + 128 + 256 * 1024, "{total_read}");
        let mut acquire = request.clone();
        acquire.insert("operation", json!("acquire"));
        let acquired = store.request(&acquire, &context, &Cancellation::default());
        assert_eq!(acquired["status"].as_str(), Some("ok"), "{acquired:?}");
        assert_eq!(
            acquired["data"]["description"]["window_start"].as_u64(),
            Some(start)
        );
        assert_eq!(acquired["usage"]["source_bytes_read"].as_u64(), Some(0));
        let whole = store.request(
            &warm_request(&path, 256 * 1024),
            &context,
            &Cancellation::default(),
        );
        assert_eq!(whole["status"].as_str(), Some("ok"), "{whole:?}");
        assert_eq!(whole["data"]["complete"].as_bool(), Some(false));
        assert!(whole["data"]["source_offset"].as_u64().unwrap() < start);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn a_trailing_window_of_a_root_past_the_source_bound_warms_and_acquires() {
        let (directory, path) = source(4 * 1024 * 1024);
        let bound = std::fs::metadata(&path).unwrap().len() - 1;
        let store = NativeStore::new(&json!({"max_read_bytes_per_step":256*1024,"max_source_bytes":bound,"max_retained_bytes":128*1024*1024,"reserved_hook_accounted_bytes":4096,"max_leases":16,"reserved_hook_leases":1})).unwrap();
        let context = context("bounded-window");
        let mut request = warm_request(&path, 256 * 1024);
        request.insert("tail_bytes", json!(256 * 1024));
        let mut finished = false;
        for _ in 0..24 {
            let reply = store.request(&request, &context, &Cancellation::default());
            assert_eq!(reply["status"].as_str(), Some("ok"), "{reply:?}");
            if reply["data"]["complete"].as_bool() == Some(true) {
                finished = true;
                break;
            }
        }
        assert!(finished);
        let mut acquire = request.clone();
        acquire.insert("operation", json!("acquire"));
        let acquired = store.request(&acquire, &context, &Cancellation::default());
        assert_eq!(acquired["status"].as_str(), Some("ok"), "{acquired:?}");
        acquire.insert("tail_bytes", json!(null));
        let whole = store.request(&acquire, &context, &Cancellation::default());
        assert_eq!(whole["status"].as_str(), Some("source_limit"), "{whole:?}");
        assert_eq!(whole["reason"].as_str(), Some("source exceeds owner bound"));
        let whole_warm = store.request(
            &warm_request(&path, 256 * 1024),
            &context,
            &Cancellation::default(),
        );
        assert_eq!(whole_warm["status"].as_str(), Some("source_limit"), "{whole_warm:?}");
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn a_base_advance_supersedes_the_previous_window_pin() {
        let (directory, path) = source(4 * 1024 * 1024);
        let store = NativeStore::new(&json!({"max_read_bytes_per_step":256*1024,"max_retained_bytes":128*1024*1024,"reserved_hook_accounted_bytes":4096,"max_leases":16,"reserved_hook_leases":1})).unwrap();
        let context = context("advancing-window");
        let mut request = warm_request(&path, 256 * 1024);
        request.insert("tail_bytes", json!(1024 * 1024));
        let first = store.request(&request, &context, &Cancellation::default());
        assert_eq!(first["status"].as_str(), Some("ok"), "{first:?}");
        assert_eq!(first["data"]["complete"].as_bool(), Some(false));
        let old_base = std::fs::metadata(&path).unwrap().len() / (512 * 1024) * (512 * 1024)
            - 1024 * 1024;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        let payload = "y".repeat(16 * 1024);
        for index in 0..48 {
            file.write_all(format!(
                "{{\"type\":\"user\",\"uuid\":\"late-{index}\",\"sessionId\":\"s\",\"timestamp\":\"2026-01-02T03:04:05Z\",\"message\":{{\"content\":\"{payload}\"}}}}\n"
            ).as_bytes()).unwrap();
        }
        drop(file);
        let size = std::fs::metadata(&path).unwrap().len();
        let new_base = (size - 1024 * 1024) / (512 * 1024) * (512 * 1024);
        assert!(new_base > old_base);
        let second = store.request(&request, &context, &Cancellation::default());
        assert_eq!(second["status"].as_str(), Some("ok"), "{second:?}");
        let state = store.state.lock().unwrap();
        let pinned: Vec<_> = state.prepared_loads.keys().copied().collect();
        assert_eq!(pinned.len(), 1, "{pinned:?}");
        assert_eq!(pinned[0].window_base, new_base);
        assert_eq!(state.loads.len(), 1);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn whole_file_and_window_warms_keep_their_own_pins() {
        let (directory, path) = source(4 * 1024 * 1024);
        let store = NativeStore::new(&json!({"max_read_bytes_per_step":256*1024,"max_retained_bytes":128*1024*1024,"reserved_hook_accounted_bytes":4096,"max_leases":16,"reserved_hook_leases":1})).unwrap();
        let context = context("alternating-views");
        let whole = warm_request(&path, 256 * 1024);
        let mut window = whole.clone();
        window.insert("tail_bytes", json!(1024 * 1024));
        let step = |request: &Value| {
            let reply = store.request(request, &context, &Cancellation::default());
            assert_eq!(reply["status"].as_str(), Some("ok"), "{reply:?}");
            assert_eq!(reply["data"]["complete"].as_bool(), Some(false), "{reply:?}");
            reply["data"]["source_offset"].as_u64().unwrap()
        };
        step(&whole);
        let whole_offset = step(&whole);
        assert!(whole_offset > 0);
        step(&window);
        let window_offset = step(&window);
        assert!(window_offset > whole_offset);
        assert_eq!(store.state.lock().unwrap().prepared_loads.len(), 2);
        let advances = |request: &Value, from: u64| {
            for _ in 0..4 {
                let offset = step(request);
                assert!(offset >= from, "{offset} < {from}");
                if offset > from {
                    return;
                }
            }
            panic!("view did not advance past {from}");
        };
        advances(&whole, whole_offset);
        advances(&window, window_offset);
        assert_eq!(store.state.lock().unwrap().prepared_loads.len(), 2);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn a_productive_deadline_keeps_the_partial_root() {
        let (directory, path) = source(1024 * 1024);
        let store = NativeStore::new(&json!({"max_read_bytes_per_step":1024*1024,"max_retained_bytes":64*1024*1024,"reserved_hook_accounted_bytes":4096,"max_leases":16,"reserved_hook_leases":1})).unwrap();
        let context = context("deadline-warm");
        *store.read_hook.lock().unwrap() = Some(Arc::new(|| {
            std::thread::sleep(std::time::Duration::from_millis(500));
        }));
        let mut request = warm_request(&path, 1024 * 1024);
        request.insert("deadline_unix_ms", json!(now_ms() + 250));
        let reply = store.request(&request, &context, &Cancellation::default());
        assert_eq!(reply["status"].as_str(), Some("ok"), "{reply:?}");
        assert_eq!(reply["data"]["complete"].as_bool(), Some(false));
        assert!(reply["usage"]["source_bytes_read"].as_u64().unwrap() > 0);
        assert!(store.state.lock().unwrap().prepared_loads.len() > 0);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn an_appending_root_finishes_its_pinned_revision_without_rereading() {
        let (directory, path) = source(4 * 1024 * 1024);
        let pinned_size = std::fs::metadata(&path).unwrap().len();
        let store = NativeStore::new(&json!({"max_read_bytes_per_step":1024*1024,"max_retained_bytes":128*1024*1024,"reserved_hook_accounted_bytes":4096,"max_leases":16,"reserved_hook_leases":1})).unwrap();
        let context = context("appending-warm");
        let request = warm_request(&path, 1024 * 1024);
        let first = store.request(&request, &context, &Cancellation::default());
        assert_eq!(first["status"].as_str(), Some("ok"), "{first:?}");
        assert_eq!(first["data"]["complete"].as_bool(), Some(false));
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"{\"type\":\"user\",\"uuid\":\"later\",\"sessionId\":\"s\",\"timestamp\":\"2026-01-02T03:04:05Z\",\"message\":{\"content\":\"later\"}}\n")
            .unwrap();
        let revision = first["data"]["source_revision"].clone();
        let mut total_read = first["usage"]["source_bytes_read"].as_u64().unwrap();
        let mut finished = false;
        for _ in 0..24 {
            let reply = store.request(&request, &context, &Cancellation::default());
            assert_eq!(reply["status"].as_str(), Some("ok"), "{reply:?}");
            assert_eq!(reply["data"]["source_revision"], revision);
            total_read += reply["usage"]["source_bytes_read"].as_u64().unwrap();
            if reply["data"]["complete"].as_bool() == Some(true) {
                assert_eq!(reply["data"]["source_offset"].as_u64(), Some(pinned_size));
                finished = true;
                break;
            }
        }
        assert!(finished);
        assert!(total_read <= pinned_size + 128);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn custom_root_facts_use_the_registered_classifier_without_source_reads() {
        let (directory, path) = source(32 * 1024);
        let store = NativeStore::new(&json!({"max_read_bytes_per_step":1024*1024,"max_retained_bytes":64*1024*1024,"reserved_hook_accounted_bytes":4096,"max_leases":16,"reserved_hook_leases":1})).unwrap();
        let context = context("classifier-warm");
        let native = warm_request(&path, 1024 * 1024);
        for _ in 0..8 {
            let reply = store.request(&native, &context, &Cancellation::default());
            assert_eq!(reply["status"].as_str(), Some("ok"), "{reply:?}");
            if reply["data"]["complete"].as_bool() == Some(true) {
                break;
            }
        }
        let physical = store
            .state
            .lock()
            .unwrap()
            .latest
            .values()
            .next()
            .cloned()
            .unwrap();
        let called = Arc::new(AtomicUsize::new(0));
        let callback_calls = Arc::clone(&called);
        store
            .register_classifier(
                "custom",
                "1",
                Arc::new(move |_, range| {
                    callback_calls.fetch_add(1, Ordering::Relaxed);
                    Ok(vec![true; range.len()])
                }),
            )
            .unwrap();
        let mut custom = warm_request(&path, 1024 * 1024);
        custom.insert("classifier", json!({"id":"custom","version":"1"}));
        let mut complete = false;
        for _ in 0..8 {
            let reply = store.request(&custom, &context, &Cancellation::default());
            assert_eq!(reply["status"].as_str(), Some("ok"), "{reply:?}");
            assert_eq!(reply["usage"]["source_bytes_read"].as_u64(), Some(0));
            if reply["data"]["complete"].as_bool() == Some(true) {
                complete = true;
                break;
            }
        }
        assert!(complete);
        assert!(called.load(Ordering::Relaxed) > 0);
        let state = store.state.lock().unwrap();
        assert!(Arc::ptr_eq(
            state.latest.values().next().unwrap(),
            &physical
        ));
        assert_eq!(
            state.prepared_facts.values().next().unwrap().classifier,
            json!({"id":"custom","version":"1"})
        );
        drop(state);
        let mut acquire = custom.clone();
        acquire.insert("operation", json!("acquire"));
        let acquired = store.request(&acquire, &context, &Cancellation::default());
        assert_eq!(acquired["status"].as_str(), Some("ok"), "{acquired:?}");
        assert_eq!(acquired["usage"]["source_bytes_read"].as_u64(), Some(0));
        let classified = store
            .pin(&acquired["data"]["description"]["handle"], &context)
            .unwrap();
        assert_ne!(classified.id, physical.id);
        let callbacks = called.load(Ordering::Relaxed);
        let repeated_native = store.request(&native, &context, &Cancellation::default());
        assert_eq!(
            repeated_native["status"].as_str(),
            Some("ok"),
            "{repeated_native:?}"
        );
        assert_eq!(repeated_native["data"]["complete"].as_bool(), Some(true));
        assert_eq!(
            repeated_native["usage"]["source_bytes_read"].as_u64(),
            Some(0)
        );
        assert_eq!(called.load(Ordering::Relaxed), callbacks);
        assert_eq!(store.prepared_disk.stats().writes, 2);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    #[ignore = "run in CI with --ignored"]
    fn two_hundred_megabyte_root_completes_in_three_second_steps() {
        let (directory, path) = source(202 * 1024 * 1024);
        let source_size = std::fs::metadata(&path).unwrap().len();
        let store = NativeStore::new(&json!({"max_read_bytes_per_step":8*1024*1024,"max_retained_bytes":1024*1024*1024,"max_source_bytes":512*1024*1024,"reserved_hook_accounted_bytes":512*1024*1024,"max_leases":16,"reserved_hook_leases":1})).unwrap();
        let context = context("ci-large-root");
        let mut request = warm_request(&path, 8 * 1024 * 1024);
        let started = std::time::Instant::now();
        let mut total_read = 0;
        let mut finished = false;
        for _ in 0..512 {
            request.insert("deadline_unix_ms", json!(now_ms() + 3_000));
            let call_started = std::time::Instant::now();
            let reply = store.request(&request, &context, &Cancellation::default());
            assert_eq!(reply["status"].as_str(), Some("ok"), "{reply:?}");
            assert!(call_started.elapsed() <= std::time::Duration::from_secs(6));
            let read = reply["usage"]["source_bytes_read"].as_u64().unwrap();
            assert!(read <= 8 * 1024 * 1024);
            total_read += read;
            if reply["data"]["facts_complete"].as_bool() == Some(true) {
                assert_eq!(reply["data"]["source_offset"].as_u64(), Some(source_size));
                finished = true;
                break;
            }
            assert!(started.elapsed() <= std::time::Duration::from_secs(180));
        }
        assert!(finished);
        assert!(total_read >= source_size);
        assert!(total_read <= source_size + 128);
        std::fs::remove_dir_all(directory).unwrap();
    }
}
