#[test]
fn load_charge_work_is_independent_of_retained_chunks() {
    let mut measurements = Vec::new();
    for count in [33, 129, 513] {
        let source = Source::new(&users(0..count + 1));
        let store = NativeStore::new(&json!({
            "max_read_bytes_per_step": 1024 * 1024,
            "max_events_per_step": 1,
            "max_retained_bytes": 128 * 1024 * 1024,
            "reserved_hook_accounted_bytes": 4096
        })).unwrap();
        let owner = context("load-charge-work");
        let mut request = acquire(&source.path);
        request["limits"]["max_events"] = json!(4096);
        let mut response = store.request(&request, &owner, &Cancellation::default());
        let mut measured = false;
        for _ in 0..4096 {
            let slot = store.lock_state().loads.values().next().cloned().unwrap();
            {
                let load = slot.work.lock().unwrap();
                if load.chunks.len() >= count {
                    assert!(load.result.is_none());
                    let work = load.charge_work.counter();
                    let before = work.load(Ordering::Relaxed);
                    let expected = NativeStore::unpublished_load_charge(&load);
                    for _ in 0..63 {
                        assert_eq!(NativeStore::unpublished_load_charge(&load), expected);
                    }
                    let examined = work.load(Ordering::Relaxed) - before;
                    eprintln!("load_charge_probe chunks={count} calls=64 allocations_examined={examined}");
                    measurements.push((count, examined));
                    measured = true;
                }
            }
            if measured {
                break;
            }
            assert_eq!(response["status"].as_str(), Some("incomplete"), "{response:?}");
            let cursor = response["cursor"].as_str().unwrap();
            response = store.request(
                &json!({"schema":SCHEMA,"id":"load-charge-resume","operation":"resume","cursor":cursor}),
                &owner,
                &Cancellation::default(),
            );
        }
        assert!(measured, "never reached {count} retained chunks");
        store.assert_conserved();
    }
    assert_eq!(measurements, vec![(33, 64), (129, 64), (513, 64)]);
}
