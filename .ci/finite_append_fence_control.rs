const FINITE_FENCE_OLD_LINE: &str = concat!(
    r#"{"message":{"content":"prompt guarded"},"uuid":"guarded","type":"user","timestamp":"2026-01-02T03:04:05Z","sessionId":"s"}"#,
    "\n",
);

fn finite_append_fence_observe(case: &str, replacement: &str) -> (String, bool) {
    let old = FINITE_FENCE_OLD_LINE;
    assert_eq!(old.len(), 123);
    assert_eq!(replacement.len(), 369);
    let fence = &old.as_bytes()[old.len() - 64..];
    assert_eq!(&replacement.as_bytes()[old.len() - 64..old.len()], fence);
    let source = LedgerSource::new(old);
    let store = fast_store();
    let owner = context_for("finite-append-fence", false);
    let (handle, prior) = acquired(&store, &source.path, &owner);
    assert!(matches!(prior.provider, Provider::Claude));
    assert_eq!(prior.event_count, 1);
    assert_eq!(prior.committed_bytes as usize, old.len());
    assert_eq!(prior.entry(0).meta().unwrap().uuid, "guarded");
    release_lease(&store, &handle, &owner);
    let before = store.lock_state().counters;
    std::fs::write(&source.path, replacement).unwrap();
    let physical = SourceStamp::of(&std::fs::metadata(&source.path).unwrap());
    assert_eq!(physical.identity.file(), prior.stamp.identity.file());
    assert!(physical.size > prior.stamp.size);
    let (_, fresh) = acquired(&store, &source.path, &owner);
    assert_eq!(fresh.event_count, 3);
    assert_eq!(prior.entry(0).meta().unwrap().uuid, "guarded");
    let after = store.lock_state().counters;
    let delta: [u64; 20] = std::array::from_fn(|index| after[index] - before[index]);
    let first = fresh.entry(0).meta().unwrap().uuid.to_string();
    let shared = Arc::ptr_eq(&prior.chunks[0], &fresh.chunks[0]);
    eprintln!(
        "FINITE_APPEND_FENCE_OBSERVATION {}",
        json!({
            "case":case,"old":old,"replacement":replacement,
            "old_bytes":old.len(),"replacement_bytes":replacement.len(),
            "old_fence":fence,"fence_equal":true,
            "prefix_equal":&replacement.as_bytes()[..old.len()]==old.as_bytes(),
            "prior_stamp":format!("{:?}",prior.stamp),
            "observed_physical_stamp":format!("{:?}",physical),
            "fresh_stamp":format!("{:?}",fresh.stamp),
            "fresh_first_uuid":first.as_str(),"prior_chunk_shared":shared,
            "native_logical_counter_delta":usage_value(&delta)
        })
    );
    store.assert_conserved();
    (first, shared)
}

#[test]
fn finite_append_fence_common_tail_rewrite_does_not_reuse_old_uuid() {
    let old = FINITE_FENCE_OLD_LINE;
    let replacement: String = (0..3)
        .map(|index| old.replace("guarded", &format!("event-{index}")))
        .collect();
    assert_ne!(&replacement.as_bytes()[..old.len()], old.as_bytes());
    let (first, shared) = finite_append_fence_observe("common_tail_rewrite", &replacement);
    assert_eq!(first, "event-0", "COMMON_TAIL_REWRITE_REUSED_PRIOR_UUID");
    assert!(!shared, "COMMON_TAIL_REWRITE_REUSED_PRIOR_CHUNK");
}

#[test]
fn finite_append_fence_true_append_preserves_prior_chunk() {
    let old = FINITE_FENCE_OLD_LINE;
    let replacement = format!(
        "{old}{}{}",
        old.replace("guarded", "event-1"),
        old.replace("guarded", "event-2")
    );
    assert_eq!(&replacement.as_bytes()[..old.len()], old.as_bytes());
    let (first, shared) = finite_append_fence_observe("true_append", &replacement);
    assert_eq!(first, "guarded");
    assert!(shared, "TRUE_APPEND_DID_NOT_PRESERVE_PRIOR_CHUNK");
}
