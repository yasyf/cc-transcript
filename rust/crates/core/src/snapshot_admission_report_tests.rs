#[test]
fn admission_report_checks_do_not_serialize_statistics() {
    let store = NativeStore::new(&json!({
        "max_retained_bytes": 128 * 1024 * 1024,
        "reserved_hook_accounted_bytes": 4096
    })).unwrap();
    let owner = context("admission-reports");
    let reports = store.lock_state().gauge_reports.counter();
    store.lock_state().ledger.shared.work().traced();
    let before = reports.load(Ordering::Relaxed);
    for _ in 0..64 {
        let reservation = store.reserve_projection(&owner, 1).unwrap();
        drop(reservation);
    }
    let report_count = reports.load(Ordering::Relaxed) - before;
    let trace = store.lock_state().ledger.shared.work().traced();
    let admitted = trace.into_iter().filter_map(|entry| match entry {
        crate::snapshot_ledger::Trace::Admitted(bytes) => Some(bytes),
        _ => None,
    }).collect::<Vec<_>>();
    eprintln!("admission_report_probe admissions={} reports={report_count}", admitted.len());
    assert_eq!(admitted, vec![1; 64]);
    assert_eq!(report_count, 0);
    let before = reports.load(Ordering::Relaxed);
    let stats = store.request(
        &json!({"schema":SCHEMA,"id":"admission-stats","operation":"stats"}),
        &owner,
        &Cancellation::default(),
    );
    assert_eq!(stats["status"].as_str(), Some("ok"), "{stats:?}");
    assert_eq!(stats["data"]["kind"].as_str(), Some("stats"));
    assert_eq!(reports.load(Ordering::Relaxed) - before, 1);
    store.assert_conserved();
}
