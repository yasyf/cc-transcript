use std::sync::Arc;

use cc_transcript_core::snapshot::{
    Cancellation, NativeStore, SnapshotError, Status, TranscriptSnapshot, WorkLimits,
};
use cc_transcript_core::snapshot_codec;
use cc_transcript_core::snapshot_projection::{self, ProjectionUsage};
use cc_transcript_core::toolcall::{with_registry, ToolRegistrySnapshot};
use sonic_rs::{json, Value};

pub struct Failure {
    pub error: SnapshotError,
    pub usage: Option<Value>,
}

impl From<SnapshotError> for Failure {
    fn from(error: SnapshotError) -> Self {
        Self { error, usage: None }
    }
}

#[derive(Default, Clone, Copy)]
pub struct ScopeWork {
    pub read_bytes: usize,
    pub events: usize,
    pub items: usize,
    pub output_bytes: usize,
}

impl ScopeWork {
    fn charge(&mut self, limits: WorkLimits, additional: Self) -> Result<(), SnapshotError> {
        for (used, amount, maximum, reason) in [
            (
                self.read_bytes,
                additional.read_bytes,
                limits.max_read_bytes,
                "read_limit",
            ),
            (
                self.events,
                additional.events,
                limits.max_events,
                "event_limit",
            ),
            (self.items, additional.items, limits.max_items, "item_limit"),
            (
                self.output_bytes,
                additional.output_bytes,
                limits.max_output_bytes,
                "output_limit",
            ),
        ] {
            if amount > maximum.saturating_sub(used) {
                return Err(SnapshotError::new(Status::Incomplete, reason));
            }
        }
        self.read_bytes += additional.read_bytes;
        self.events += additional.events;
        self.items += additional.items;
        self.output_bytes += additional.output_bytes;
        Ok(())
    }

    fn remaining(self, limits: WorkLimits) -> WorkLimits {
        WorkLimits {
            max_read_bytes: limits.max_read_bytes.saturating_sub(self.read_bytes),
            max_events: limits.max_events.saturating_sub(self.events),
            max_items: limits.max_items.saturating_sub(self.items),
            max_output_bytes: limits.max_output_bytes.saturating_sub(self.output_bytes),
            ..limits
        }
    }
}

pub struct Scope<'a> {
    pub store: &'a NativeStore,
    pub context: &'a Value,
    pub snapshot: Arc<TranscriptSnapshot>,
    pub registry: Arc<ToolRegistrySnapshot>,
    pub limits: WorkLimits,
    pub cancel: &'a Cancellation,
    handle: Value,
    description: Value,
    used: ScopeWork,
    materialized_output_bytes: usize,
    failures: usize,
    cancellations: usize,
}

impl<'a> Scope<'a> {
    pub fn borrow(
        store: &'a NativeStore,
        handle: Value,
        context: &'a Value,
        limits: WorkLimits,
        cancel: &'a Cancellation,
    ) -> Result<Self, SnapshotError> {
        cancel.check(limits.deadline_unix_ms)?;
        let (snapshot, description) =
            store.pin_scope_for_work(&handle, context, limits.deadline_unix_ms)?;
        let registry = store.registry_for_scope(&handle, context)?;
        cancel.check(limits.deadline_unix_ms)?;
        Ok(Self {
            store,
            context,
            snapshot,
            registry,
            limits,
            cancel,
            handle,
            description,
            used: ScopeWork::default(),
            materialized_output_bytes: 0,
            failures: 0,
            cancellations: 0,
        })
    }

    pub fn remaining(&self) -> WorkLimits {
        let mut remaining = self.used.remaining(self.limits);
        remaining.max_output_bytes = remaining.max_output_bytes.min(
            self.limits
                .max_output_bytes
                .saturating_sub(self.materialized_output_bytes),
        );
        remaining
    }

    pub fn fail(&mut self, error: SnapshotError) -> Failure {
        self.failures += 1;
        if error.status == Status::Cancelled {
            self.cancellations += 1;
        }
        Failure {
            error,
            usage: Some(self.usage()),
        }
    }

    pub fn checkpoint(&mut self) -> Result<(), Failure> {
        self.check().map_err(|error| self.fail(error))
    }

    fn check(&self) -> Result<(), SnapshotError> {
        self.cancel.check(self.limits.deadline_unix_ms)?;
        self.store.validate_scope(&self.handle, self.context)
    }

    pub fn charge(&mut self, additional: ScopeWork) -> Result<(), Failure> {
        self.used
            .charge(self.limits, additional)
            .map_err(|error| self.fail(error))
    }

    pub fn materialize(&mut self, bytes: usize) {
        self.materialized_output_bytes = self.materialized_output_bytes.saturating_add(bytes);
    }

    pub fn description(&mut self) -> Result<Value, Failure> {
        self.checkpoint()?;
        let limit = self.remaining().max_output_bytes;
        let encoded = snapshot_codec::encoded_size(&self.description, limit)
            .map_err(|error| self.fail(error))?;
        self.materialize(encoded);
        self.checkpoint()?;
        Ok(self.description.clone())
    }

    pub fn classifier_facts(&mut self, prefix: &str, event_limit: usize) -> Result<Value, Failure> {
        self.checkpoint()?;
        if prefix.len()
            > self
                .remaining()
                .max_read_bytes
                .min(snapshot_codec::MAX_PAGE_BYTES)
        {
            return Err(self.fail(SnapshotError::new(
                Status::SourceLimit,
                "classifier prefix exceeds input budget",
            )));
        }
        self.charge(ScopeWork {
            read_bytes: prefix.len(),
            items: 1,
            ..ScopeWork::default()
        })?;
        let limits = self.remaining();
        let mut usage = ProjectionUsage::default();
        let value = with_registry(Arc::clone(&self.registry), || {
            snapshot_projection::classifier_facts(
                &self.snapshot,
                prefix,
                event_limit,
                &limits,
                self.cancel,
                &mut usage,
            )
        });
        self.charge(ScopeWork {
            read_bytes: usage.read_bytes,
            events: usage.events,
            ..ScopeWork::default()
        })?;
        let value = value.map_err(|error| self.fail(error))?;
        let encoded = snapshot_codec::encoded_size(&value, limits.max_output_bytes)
            .map_err(|error| self.fail(error))?;
        self.materialize(encoded);
        self.checkpoint()?;
        Ok(value)
    }

    pub fn usage(&self) -> Value {
        json!({
            "source_opens":0,"source_bytes_read":0,"bytes_decoded":0,"events_parsed":0,
            "cold_parses":0,"append_parses":0,"activity_lifts":0,"cache_hits":0,"inflight_joins":0,
            "generations_published":0,"generations_invalidated":0,
            "requests_cancelled":self.cancellations,"requests_failed":self.failures,
            "output_bytes":self.used.output_bytes,"transport_bytes":0,
            "nonincremental_lowering_calls":0,"nonincremental_lowering_source_bytes":0,
            "discovery_entries_examined":0,
            "prepared_cache_reads":0,"prepared_cache_bytes_read":0
        })
    }
}
