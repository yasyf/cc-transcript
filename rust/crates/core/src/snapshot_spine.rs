impl NativeStore {
    fn reuse_source_chunk(&self, slot: &LoadSlot, load: &mut Load, context: &Value) -> Result<bool, SnapshotError> {
        if !load.reuse_prepared {
            load.reuse_prepared = true;
            load.reuse_next = load.previous_chunks.as_ref().and_then(|spans| {
                let first = spans.partition_point(|span| span.start < load.window_start.unwrap());
                let previous = load.previous.as_ref().unwrap();
                let count = previous.chunks.len() - usize::from(previous.provisional_tail);
                (first < count).then_some(first)
            });
        }
        let Some(index) = load.reuse_next else { return Ok(false) };
        let span = load.previous_chunks.as_ref().unwrap()[index];
        if load.offset != span.start || !load.pending.is_empty() { return Ok(false) }
        let previous = load.previous.as_ref().unwrap();
        let chunk = &previous.chunks[index];
        let additional = vec_growth(&load.chunks, 1) + vec_growth(&load.source_chunks, 1)
            + arc_bytes::<EntryChunk>() + chunk.entry_charges.len() * size_of::<MemoryCharge>()
            + if load.session_id.is_none() {
                chunk.entries.iter().find_map(Entry::meta).map_or(0, |meta| meta.session_id.len())
            } else { 0 }
            + if span.end == previous.committed_bytes { previous.fence.capacity() } else { 0 };
        self.extend_load_reservation(slot, context, additional)?;
        if load.session_id.is_none() {
            load.session_id = chunk.entries.iter().find_map(|entry| entry.meta().map(|meta| meta.session_id.clone()));
        }
        let selected = Arc::new(chunk.rebased(load.count));
        load.fresh_chunk_bytes += selected.metadata_bytes();
        load.chunks.push(selected);
        load.count += chunk.entries.len();
        load.source_chunks.push(span);
        load.offset = span.end;
        load.pending_start = span.end;
        load.committed = span.end;
        load.provider = Some(Provider::Claude);
        if span.end == previous.committed_bytes { load.fence = previous.fence.clone() }
        let count = previous.chunks.len() - usize::from(previous.provisional_tail);
        load.reuse_next = (index + 1 < count).then_some(index + 1);
        load.file.seek(SeekFrom::Start(span.end)).map_err(io_error)?;
        Ok(true)
    }
}
