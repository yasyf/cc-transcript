impl NativeStore {
    fn warm_root(
        &self,
        request: &Value,
        context: &Value,
        cancel: &Cancellation,
        usage: &mut [u64; 18],
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
        let current = SourceStamp::of(&metadata);
        if current.size > self.config.source as u64 {
            return Err(SnapshotError::new(
                Status::SourceLimit,
                "root source exceeds owner bound",
            ));
        }
        let pinned = {
            let mut state = self.state.lock().expect("snapshot state");
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
                    state.loads.remove(&current.identity);
                    None
                }
            } else {
                None
            }
        };
        let stamp = pinned.as_ref().map_or(current, |slot| slot.stamp);
        let outcome = if let Some(slot) = pinned {
            self.resume_warm_root(slot, classifier, context, bounds, cancel, usage)
        } else {
            self.acquire(request, context, cancel, usage)
        };
        let pending = outcome
            .as_ref()
            .ok()
            .and_then(|(_, token, _)| token.clone());
        if let Some(token) = &pending {
            self.state
                .lock()
                .expect("snapshot state")
                .waiters
                .remove(token);
        }
        let slot = {
            let mut state = self.state.lock().expect("snapshot state");
            let slot = state.loads.get(&stamp.identity).cloned();
            if pending.is_some()
                || outcome.as_ref().err().is_some_and(|error| {
                    matches!(error.status, Status::Deadline | Status::Cancelled)
                })
            {
                if let Some(slot) = &slot {
                    state
                        .prepared_loads
                        .insert(stamp.identity, (Arc::clone(slot), now_ms()));
                }
            } else {
                state.prepared_loads.remove(&stamp.identity);
            }
            slot
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
        self.state
            .lock()
            .expect("snapshot state")
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
        match self.prepared_root_facts(&snapshot, classifier, context, &bounds, cancel) {
            Ok(_) => Ok(progress(true)),
            Err(error) if error.status == Status::Deadline => Ok(progress(false)),
            Err(error) => Err(error),
        }
    }

    fn resume_warm_root(
        &self,
        slot: Arc<LoadSlot>,
        classifier: &Value,
        context: &Value,
        bounds: WorkLimits,
        cancel: &Cancellation,
        usage: &mut [u64; 18],
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
            used_events: 0,
            busy: false,
        };
        {
            let mut state = self.state.lock().expect("snapshot state");
            Self::prune(&mut state);
            if state.waiters.len() >= self.lease_cap(context)? {
                return Err(SnapshotError::new(
                    Status::LeaseLimit,
                    "root warming reservation admission exhausted",
                ));
            }
            state.waiters.insert(token.clone(), waiter.clone());
        }
        self.advance(&token, waiter, cancel, usage)
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
        json!({"schema":SCHEMA,"id":"warm-root","operation":"warm_root","path":path.to_string_lossy().as_ref(),"classifier":{"id":"native","version":"1"},"deadline_unix_ms":now_ms()+30_000,"limits":{"max_read_bytes":read_bytes,"max_events":100_000,"max_items":256,"max_output_bytes":1024*1024,"max_discovery_entries":1000,"max_sources":100}})
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
    fn a_productive_deadline_keeps_the_partial_root() {
        let (directory, path) = source(1024 * 1024);
        let store = NativeStore::new(&json!({"max_read_bytes_per_step":1024*1024,"max_retained_bytes":64*1024*1024,"reserved_hook_accounted_bytes":4096,"max_leases":16,"reserved_hook_leases":1})).unwrap();
        let context = context("deadline-warm");
        *store.read_hook.lock().unwrap() = Some(Arc::new(|| {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }));
        let mut request = warm_request(&path, 1024 * 1024);
        request.insert("deadline_unix_ms", json!(now_ms() + 5));
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
        let store = NativeStore::new(&json!({"max_read_bytes_per_step":8*1024*1024,"max_retained_bytes":1536*1024*1024,"max_source_bytes":512*1024*1024,"reserved_hook_accounted_bytes":4096,"max_leases":16,"reserved_hook_leases":1})).unwrap();
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
