#[test]
fn public_catalog_indexed_positive_is_a_zero_discovery_cache_hit() {
    let source = Source::new("");
    let store = store();
    let owner = context("public-indexed-zero-discovery");
    let missing = public_catalog_request(&source, &["absent"]);
    let (rows, cold_entries) = public_catalog_finish(&store, &missing, &owner);
    assert_eq!(rows[0]["status"].as_str(), Some("missing"));
    assert_eq!(cold_entries, 1);
    assert!(!store.lock_state().locations.contains_key("s"));
    let mut selected = public_catalog_request(&source, &["s"]);
    selected["limits"].insert("max_discovery_entries", json!(0));
    let response = store.request(&selected, &owner, &Cancellation::default());
    assert_eq!(response["status"].as_str(), Some("ok"), "{response:?}");
    assert!(response["cursor"].is_null(), "{response:?}");
    assert_eq!(response["usage"]["discovery_entries_examined"].as_u64(), Some(0));
    assert_eq!(response["data"]["sessions"][0]["status"].as_str(), Some("ok"));
    assert_eq!(response["data"]["sessions"][0]["path"], json!(source.path));
    store.assert_conserved();
}
