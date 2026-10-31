#[test]
fn raw_excerpt_sparse_parse_matches_selected_bytes_and_reuses_owned_rows() {
    let source = Source::new(&format!("{}\n", user("first")));
    let store = store();
    let owner = context("excerpt");
    let response = finish(&store, store.request(&acquire(&source.path), &owner, &Cancellation::default()), &owner);
    let body = (0..100).map(|index| format!("{}\n", user(&format!("event-{index}")))).collect::<String>();
    source.append(&body);
    let mut usage = [0; 20];
    let raw = store.raw_source_scope(handle(&response), 16*1024*1024, &owner, &Cancellation::default(), raw_scope_bounds(), &mut usage).unwrap();
    let forms: &[&[u8]] = &[b"event-2", b"event-8"];
    let ranges = raw.window.select(forms, 2, 20);
    let selected = ranges.into_iter().flat_map(|range| raw.window.selected_bytes(range).iter().copied()).collect::<Vec<_>>();
    let expected = crate::parse::parse_bytes(&selected, |_| true).unwrap();
    let scope = store.raw_excerpt_scope(handle(&response), forms, 2, 20, &owner, &Cancellation::default(), raw_scope_bounds(), &mut usage).unwrap();
    assert_eq!(sonic_rs::to_string(&scope.entries()).unwrap(), sonic_rs::to_string(&expected).unwrap());
    let mut warm_usage = [0; 20];
    let warm = store.raw_excerpt_scope(handle(&response), forms, 2, 20, &owner, &Cancellation::default(), raw_scope_bounds(), &mut warm_usage).unwrap();
    assert_eq!((warm_usage[1], warm_usage[2], warm_usage[3]), (0, 0, 0));
    assert!(Arc::ptr_eq(&scope, &warm));
    store.assert_conserved();
    source.append(&format!("{}\n", user("No")));
    let current = store.raw_excerpt_scope(handle(&response), &[b"\"No\""], 2, 20, &owner, &Cancellation::default(), raw_scope_bounds(), &mut warm_usage).unwrap();
    assert!(!Arc::ptr_eq(&scope, &current));
    assert_ne!(scope.stamp, current.stamp);
    assert!(!sonic_rs::to_string(&scope.entries()).unwrap().contains(r#""No""#));
    assert!(sonic_rs::to_string(&current.entries()).unwrap().contains(r#""No""#));
    store.assert_conserved();
}
