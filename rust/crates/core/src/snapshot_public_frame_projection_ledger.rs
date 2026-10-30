pub(super) fn assert_public_frame_projection_ledger(store: &NativeStore) {
    assert_eq!(ledger(store),audited(store));
    store.assert_conserved();
}
