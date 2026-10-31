impl NativeStore {
    fn raw_excerpt_scope(
        &self, handle: &Value, forms: &[&[u8]], around: usize, limit: usize,
        context: &Value, cancel: &Cancellation, bounds: WorkLimits, usage: &mut [u64; 20],
    ) -> Result<Arc<TranscriptSnapshot>, SnapshotError> {
        let raw = self.raw_source_scope(handle, 16 * 1024 * 1024, context, cancel, bounds, usage)?;
        self.raw_excerpt_from_scope(handle, &raw, forms, around, limit, context, cancel, bounds, usage)
    }

    fn raw_excerpt_from_scope(
        &self, handle: &Value, raw: &RawSourceScope, forms: &[&[u8]], around: usize, limit: usize,
        context: &Value, cancel: &Cancellation, bounds: WorkLimits, usage: &mut [u64; 20],
    ) -> Result<Arc<TranscriptSnapshot>, SnapshotError> {
        let source = self.raw_projection_source(handle, raw, context, cancel, bounds)?;
        let key_bound = forms.iter().map(|form| form.len() + size_of::<u64>()).sum::<usize>()
            + 2 * size_of::<u64>();
        let reservation = self.reserve_projection(context, key_bound)?;
        let mut key = Vec::with_capacity(key_bound);
        key.extend_from_slice(&(around as u64).to_le_bytes());
        key.extend_from_slice(&(limit as u64).to_le_bytes());
        for form in forms {
            key.extend_from_slice(&(form.len() as u64).to_le_bytes());
            key.extend_from_slice(form);
        }
        self.raw_materialize_scope(
            handle, source, raw, RawProjectionKind::Excerpt, ReservedRawKey { key, reservation },
            raw.window.selection_work_bound(limit),
            || raw.window.select(forms, around, limit).into_iter().flat_map(move |range| {
                raw.window.selected_bytes(range).split_inclusive(|byte| *byte == b'\n')
            }),
            context, cancel, bounds, usage,
        )
    }

    fn raw_tail_from_scope(
        &self, handle: &Value, raw: &RawSourceScope, count: usize,
        context: &Value, cancel: &Cancellation, bounds: WorkLimits, usage: &mut [u64; 20],
    ) -> Result<Arc<TranscriptSnapshot>, SnapshotError> {
        let source = self.raw_projection_source(handle, raw, context, cancel, bounds)?;
        let reservation = self.reserve_projection(context, size_of::<u64>())?;
        let mut key = Vec::with_capacity(size_of::<u64>());
        key.extend_from_slice(&(count as u64).to_le_bytes());
        self.raw_materialize_scope(
            handle, source, raw, RawProjectionKind::Tail(count), ReservedRawKey { key, reservation },
            0, || raw.window.lines_rev(), context, cancel, bounds, usage,
        )
    }
}
