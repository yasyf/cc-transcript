use std::collections::VecDeque;
use std::io::{self, Write};
use std::mem::size_of;

use serde::ser::{SerializeMap, SerializeSeq};
use serde::{Serialize, Serializer};
use sha2::{Digest, Sha256};
use sonic_rs::{JsonContainerTrait, JsonValueTrait, Value};

use crate::ids::canonical_json;
use crate::snapshot::{
    now_ms, Cancellation, NativeStore, ProjectionReservation, SnapshotError, Status,
    MAX_REPLY_BYTES,
};
use crate::snapshot_codec::write_json;
use crate::snapshot_memory::value_charge;

pub const DOMAIN_PREFIX: &str = "domain-projection:";
pub const MAX_OWNED_RECORDS: usize = 65_536;
pub const MAX_OWNED_BYTES: usize = 16 * 1024 * 1024;
const MAX_PAGE_RECORDS: usize = 256;

fn invalid(reason: impl Into<String>) -> SnapshotError {
    SnapshotError::new(Status::InvalidRequest, reason)
}
fn stale() -> SnapshotError {
    SnapshotError::new(Status::StaleCursor, "owned projection cursor is stale")
}
fn output_limit() -> SnapshotError {
    SnapshotError::new(Status::OutputLimit, "owned projection output limit")
}
fn checked_add(left: usize, right: usize) -> Result<usize, SnapshotError> {
    left.checked_add(right).ok_or_else(output_limit)
}
fn number(value: &Value, key: &str) -> Result<usize, SnapshotError> {
    value
        .get(key)
        .and_then(Value::as_u64)
        .and_then(|value| value.try_into().ok())
        .ok_or_else(|| invalid(format!("missing count {key}")))
}
fn text<'a>(value: &'a Value, key: &str) -> Result<&'a str, SnapshotError> {
    value
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| invalid(format!("missing string {key}")))
}
fn object(value: &Value) -> Result<(), SnapshotError> {
    if value.is_object() {
        Ok(())
    } else {
        Err(invalid("projection metadata and counters must be objects"))
    }
}
fn charge(value: &Value) -> usize {
    let charge = value_charge(value);
    charge
        .owned_capacity_bytes
        .saturating_add(charge.opaque_dom_accounted_bytes)
}
fn context_hash(context: &Value) -> Result<[u8; 32], SnapshotError> {
    let encoded = canonical_json(context).map_err(invalid)?;
    Ok(Sha256::digest(encoded.as_bytes()).into())
}

pub struct OwnedInputReservation<'a> {
    _reservation: ProjectionReservation<'a>,
}

pub struct OwnedProjectionReply<'a> {
    encoded: String,
    _reservation: ProjectionReservation<'a>,
}

impl OwnedProjectionReply<'_> {
    pub fn json(&self) -> &str {
        &self.encoded
    }
}

#[derive(Clone, Copy)]
struct PagePlan {
    count: usize,
    bytes: usize,
    published: usize,
}

struct OwnedOperation {
    token: String,
    claimant: String,
    context_hash: [u8; 32],
    handles: Vec<Value>,
    expires: u64,
    deadline: u64,
    cancel: Cancellation,
    metadata: Value,
    field: String,
    records: VecDeque<Box<str>>,
    usage: Value,
    work: Value,
    plans: VecDeque<PagePlan>,
    next: usize,
    published: usize,
    limit: usize,
    accounted: usize,
}

impl OwnedOperation {
    fn accounted_bytes(&self) -> usize {
        self.accounted
    }

    fn retained_charge(&self) -> usize {
        size_of::<OwnedLink>()
            + self.token.capacity()
            + self.claimant.capacity()
            + self.field.capacity()
            + self.handles.capacity() * size_of::<Value>()
            + self.handles.iter().map(charge).sum::<usize>()
            + charge(&self.metadata)
            + charge(&self.usage)
            + charge(&self.work)
            + self.records.capacity() * size_of::<Box<str>>()
            + self
                .records
                .iter()
                .map(|record| record.len())
                .sum::<usize>()
            + self.plans.capacity() * size_of::<PagePlan>()
            + size_of::<std::sync::atomic::AtomicBool>()
            + 2 * size_of::<std::sync::atomic::AtomicUsize>()
    }

    fn valid(
        &self,
        offset: usize,
        claimant: &str,
        binding: &[u8; 32],
        now: u64,
    ) -> Result<(), SnapshotError> {
        if self.next != offset
            || self.claimant != claimant
            || self.context_hash != *binding
            || self.expires <= now
        {
            return Err(stale());
        }
        self.cancel.check(self.deadline)
    }

    fn binds_lease(&self, lease: &str) -> bool {
        self.handles
            .iter()
            .any(|handle| handle.get("lease_id").and_then(Value::as_str) == Some(lease))
    }
}

struct OwnedLink {
    operation: OwnedOperation,
    next: Option<Box<OwnedLink>>,
}

#[derive(Default)]
pub struct OwnedProjections {
    head: Option<Box<OwnedLink>>,
    accounted: usize,
}

impl Drop for OwnedProjections {
    fn drop(&mut self) {
        while let Some(mut node) = self.head.take() {
            self.head = node.next.take();
        }
    }
}

impl OwnedProjections {
    pub fn accounted_bytes(&self) -> usize {
        self.accounted
    }

    fn find(&self, token: &str) -> Option<&OwnedOperation> {
        let mut link = self.head.as_deref();
        while let Some(node) = link {
            if node.operation.token == token {
                return Some(&node.operation);
            }
            link = node.next.as_deref();
        }
        None
    }

    fn take(&mut self, token: &str) -> Option<OwnedOperation> {
        let mut link = &mut self.head;
        loop {
            let matches = link.as_ref().map(|node| node.operation.token == token)?;
            if matches {
                let mut node = link.take().expect("matching node");
                *link = node.next.take();
                self.accounted -= node.operation.accounted_bytes();
                return Some(node.operation);
            }
            link = &mut link.as_mut().expect("existing node").next;
        }
    }

    fn insert(&mut self, operation: OwnedOperation) -> Result<(), SnapshotError> {
        if self.find(&operation.token).is_some() {
            return Err(invalid("owned operation token collision"));
        }
        self.accounted += operation.accounted_bytes();
        self.head = Some(Box::new(OwnedLink {
            operation,
            next: self.head.take(),
        }));
        Ok(())
    }

    fn remove_if(&mut self, mut remove: impl FnMut(&OwnedOperation) -> bool) {
        let mut link = &mut self.head;
        while link.is_some() {
            if remove(&link.as_ref().expect("existing node").operation) {
                let mut node = link.take().expect("existing node");
                *link = node.next.take();
                self.accounted -= node.operation.accounted_bytes();
            } else {
                link = &mut link.as_mut().expect("existing node").next;
            }
        }
    }

    pub fn prune(&mut self, now: u64) {
        self.remove_if(|operation| {
            operation.expires <= now
                || operation.deadline <= now
                || operation.cancel.check(u64::MAX).is_err()
        });
    }
    pub fn release_lease(&mut self, lease: &str) {
        self.remove_if(|operation| operation.binds_lease(lease));
    }

    pub fn discard_cursor(&mut self, cursor: &str, claimant: &str) -> Result<bool, SnapshotError> {
        if !cursor.starts_with(DOMAIN_PREFIX) {
            return Ok(false);
        }
        let (token, offset) = split_cursor(cursor)?;
        let Some(operation) = self.find(token) else {
            return Ok(false);
        };
        if operation.claimant != claimant {
            return Err(stale());
        }
        if operation.next != offset {
            return Ok(false);
        }
        Ok(self.take(token).is_some())
    }
}

fn split_cursor(cursor: &str) -> Result<(&str, usize), SnapshotError> {
    let (token, offset) = cursor.rsplit_once(':').ok_or_else(stale)?;
    if !token.starts_with(DOMAIN_PREFIX) {
        return Err(stale());
    }
    Ok((token, offset.parse().map_err(|_| stale())?))
}

#[derive(Clone, Copy)]
enum Records<'a> {
    Borrowed(&'a [&'a str]),
    Owned(&'a VecDeque<Box<str>>),
}
impl<'a> Records<'a> {
    fn len(self) -> usize {
        match self {
            Self::Borrowed(records) => records.len(),
            Self::Owned(records) => records.len(),
        }
    }
    fn at(self, index: usize) -> &'a str {
        match self {
            Self::Borrowed(records) => records[index],
            Self::Owned(records) => &records[index],
        }
    }
}

struct RecordWindow<'a> {
    records: Records<'a>,
    start: usize,
    count: usize,
}
impl Serialize for RecordWindow<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut sequence = serializer.serialize_seq(Some(self.count))?;
        for index in self.start..self.start + self.count {
            sequence.serialize_element(self.records.at(index))?;
        }
        sequence.end()
    }
}

struct UsageWire<'a> {
    base: &'a Value,
    published: usize,
}
impl Serialize for UsageWire<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(None)?;
        for (key, value) in self.base.as_object().expect("validated usage").iter() {
            if key != "output_bytes" {
                map.serialize_entry(key, value)?;
            }
        }
        map.serialize_entry("output_bytes", &self.published)?;
        map.end()
    }
}

struct WorkWire<'a> {
    base: &'a Value,
    producer_output: usize,
    published: usize,
}
impl Serialize for WorkWire<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(None)?;
        for (key, value) in self.base.as_object().expect("validated work").iter() {
            if key != "published_output_bytes" && key != "produced_output_bytes" {
                map.serialize_entry(key, value)?;
            }
        }
        map.serialize_entry("published_output_bytes", &self.published)?;
        if self.base.get("produced_output_bytes").is_some()
            || self.base.get("output_bytes").and_then(Value::as_u64)
                != Some(self.producer_output as u64)
        {
            map.serialize_entry("produced_output_bytes", &self.producer_output)?;
        }
        map.end()
    }
}

#[derive(Serialize)]
struct PageWire<'a> {
    metadata: &'a Value,
    field: &'a str,
    records_json: RecordWindow<'a>,
    complete: bool,
    cursor: Option<&'a str>,
    usage: UsageWire<'a>,
    work: WorkWire<'a>,
}

struct PageSource<'a> {
    metadata: &'a Value,
    field: &'a str,
    records: Records<'a>,
    usage: &'a Value,
    work: &'a Value,
    token: &'a str,
    base: usize,
    limit: usize,
}

impl PageSource<'_> {
    fn wire<'a>(
        &'a self,
        start: usize,
        count: usize,
        published: usize,
        cursor: Option<&'a str>,
    ) -> PageWire<'a> {
        PageWire {
            metadata: self.metadata,
            field: self.field,
            records_json: RecordWindow {
                records: self.records,
                start,
                count,
            },
            complete: start + count == self.records.len(),
            cursor,
            usage: UsageWire {
                base: self.usage,
                published,
            },
            work: WorkWire {
                base: self.work,
                producer_output: self
                    .usage
                    .get("output_bytes")
                    .and_then(Value::as_u64)
                    .expect("validated usage") as usize,
                published,
            },
        }
    }

    fn measure(
        &self,
        start: usize,
        count: usize,
        before: usize,
        cancel: &Cancellation,
        deadline: u64,
    ) -> Result<PagePlan, SnapshotError> {
        let cursor = (start + count < self.records.len())
            .then(|| format!("{}:{}", self.token, self.base + start + count));
        let mut published = before;
        loop {
            cancel.check(deadline)?;
            let wire = self.wire(start, count, published, cursor.as_deref());
            let mut counter = PageCounter {
                bytes: 0,
                limit: MAX_REPLY_BYTES,
            };
            write_json(&mut counter, &wire, MAX_REPLY_BYTES).map_err(|_| output_limit())?;
            let updated = checked_add(before, counter.bytes)?;
            if updated > self.limit {
                return Err(output_limit());
            }
            if updated == published {
                return Ok(PagePlan {
                    count,
                    bytes: counter.bytes,
                    published,
                });
            }
            published = updated;
        }
    }

    fn plans(
        &self,
        cancel: &Cancellation,
        deadline: u64,
    ) -> Result<VecDeque<PagePlan>, SnapshotError> {
        let mut pages = VecDeque::new();
        let mut start = 0;
        let mut published = 0;
        loop {
            cancel.check(deadline)?;
            let remaining = self.records.len() - start;
            if remaining == 0 {
                if start == 0 {
                    pages.push_back(self.measure(0, 0, published, cancel, deadline)?);
                }
                break;
            }
            let mut low = 1;
            let mut high = remaining.min(MAX_PAGE_RECORDS);
            let mut best = None;
            while low <= high {
                let count = low + (high - low) / 2;
                match self.measure(start, count, published, cancel, deadline) {
                    Ok(plan) => {
                        best = Some(plan);
                        low = count + 1;
                    }
                    Err(error) if error.status == Status::OutputLimit => {
                        high = count - 1;
                    }
                    Err(error) => return Err(error),
                }
            }
            let plan = best.ok_or_else(output_limit)?;
            start += plan.count;
            published = plan.published;
            pages.push_back(plan);
        }
        Ok(pages)
    }
}

struct PageCounter {
    bytes: usize,
    limit: usize,
}
impl Write for PageCounter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > self.limit.saturating_sub(self.bytes) {
            return Err(io::Error::other("owned page limit"));
        }
        self.bytes += bytes.len();
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct PageBuffer {
    bytes: Vec<u8>,
}
impl Write for PageBuffer {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > MAX_REPLY_BYTES.saturating_sub(self.bytes.len()) {
            return Err(io::Error::other("owned page limit"));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn page(operation: &mut OwnedOperation, cancel: &Cancellation) -> Result<String, SnapshotError> {
    operation.cancel.check(operation.deadline)?;
    cancel.check(operation.deadline)?;
    let plan = operation.plans.pop_front().expect("unconsumed page");
    assert_eq!(plan.published, operation.published + plan.bytes);
    let source = PageSource {
        metadata: &operation.metadata,
        field: &operation.field,
        records: Records::Owned(&operation.records),
        usage: &operation.usage,
        work: &operation.work,
        token: &operation.token,
        base: operation.next,
        limit: operation.limit,
    };
    let cursor = (!operation.plans.is_empty())
        .then(|| format!("{}:{}", operation.token, operation.next + plan.count));
    let wire = source.wire(0, plan.count, plan.published, cursor.as_deref());
    let mut output = PageBuffer {
        bytes: Vec::with_capacity(plan.bytes),
    };
    write_json(&mut output, &wire, MAX_REPLY_BYTES).map_err(|_| output_limit())?;
    assert_eq!(output.bytes.len(), plan.bytes);
    operation.cancel.check(operation.deadline)?;
    cancel.check(operation.deadline)?;
    operation.accounted -= operation
        .records
        .iter()
        .take(plan.count)
        .map(|record| record.len())
        .sum::<usize>();
    operation.records.drain(..plan.count);
    operation.next += plan.count;
    operation.published = plan.published;
    Ok(String::from_utf8(output.bytes).expect("JSON UTF-8"))
}

fn admitted<'a, T>(
    store: &'a NativeStore,
    context: &Value,
    bytes: usize,
    prepare: impl FnOnce() -> Result<T, SnapshotError>,
) -> Result<(ProjectionReservation<'a>, T), SnapshotError> {
    let reservation = store.reserve_projection(context, bytes)?;
    Ok((reservation, prepare()?))
}

fn source_handles(request: &Value) -> Result<Vec<Value>, SnapshotError> {
    let handle = request
        .get("view")
        .and_then(|view| view.get("handle"))
        .ok_or_else(|| invalid("publication requires source handle"))?;
    Ok(vec![handle.clone()])
}

fn validate_inputs(
    request: &Value,
    metadata: &Value,
    field: &str,
    records: &[&str],
    usage: &Value,
    work: &Value,
) -> Result<(u64, usize), SnapshotError> {
    object(metadata)?;
    object(usage)?;
    object(work)?;
    if field.is_empty() || field.len() > 256 {
        return Err(invalid("invalid publication field"));
    }
    if records.len() > MAX_OWNED_RECORDS {
        return Err(output_limit());
    }
    let raw_bytes = records
        .iter()
        .try_fold(0usize, |total, record| checked_add(total, record.len()))?;
    if raw_bytes > MAX_OWNED_BYTES {
        return Err(output_limit());
    }
    let limits = request
        .get("limits")
        .ok_or_else(|| invalid("publication requires limits"))?;
    for (used, limit) in [
        ("read_bytes", "max_read_bytes"),
        ("events", "max_events"),
        ("items", "max_items"),
        ("output_bytes", "max_output_bytes"),
    ] {
        if number(work, used)? > number(limits, limit)? {
            return Err(SnapshotError::new(
                Status::Incomplete,
                "producer work exceeded publication limits",
            ));
        }
    }
    if records.len() > number(limits, "max_items")? {
        return Err(SnapshotError::new(
            Status::Incomplete,
            "publication item limit",
        ));
    }
    if work
        .get("published_output_bytes")
        .and_then(Value::as_u64)
        .unwrap_or(0)
        != 0
    {
        return Err(invalid(
            "published work cannot start another owned operation",
        ));
    }
    let producer_output = number(usage, "output_bytes")?;
    if work.get("produced_output_bytes").is_some()
        && number(work, "produced_output_bytes")? != producer_output
    {
        return Err(invalid("producer output accounting differs"));
    }
    text(request, "id")?;
    Ok((
        number(request, "deadline_unix_ms")? as u64,
        number(limits, "max_output_bytes")?.min(MAX_OWNED_BYTES),
    ))
}

impl NativeStore {
    pub fn reserve_owned_input<'a>(
        &'a self,
        context: &Value,
        cancel: &Cancellation,
        staging_bytes: usize,
    ) -> Result<OwnedInputReservation<'a>, SnapshotError> {
        cancel.check(u64::MAX)?;
        if staging_bytes > MAX_OWNED_BYTES * 8 {
            return Err(output_limit());
        }
        Ok(OwnedInputReservation {
            _reservation: self.reserve_projection(context, staging_bytes)?,
        })
    }

    pub fn publish_projection<'a>(
        &'a self,
        request: &Value,
        context: &Value,
        cancel: &Cancellation,
        metadata: &Value,
        field: &str,
        records_json: &[&str],
        usage: &Value,
        work: &Value,
    ) -> Result<OwnedProjectionReply<'a>, SnapshotError> {
        let (deadline, limit) =
            validate_inputs(request, metadata, field, records_json, usage, work)?;
        cancel.check(deadline)?;
        let input = records_json.iter().try_fold(field.len(), |bytes, record| {
            checked_add(bytes, record.len())
        })?;
        let input = [request, context, metadata, usage, work]
            .iter()
            .try_fold(input, |bytes, value| checked_add(bytes, charge(value)))?;
        let vectors = records_json
            .len()
            .checked_mul(size_of::<Box<str>>() + size_of::<PagePlan>() + 2 * size_of::<&str>())
            .ok_or_else(output_limit)?;
        let reservation_bytes = checked_add(
            checked_add(
                input.checked_mul(3).ok_or_else(output_limit)?,
                vectors.checked_mul(2).ok_or_else(output_limit)?,
            )?,
            MAX_REPLY_BYTES * 6 + 8192,
        )?;
        let (mut reservation, mut operation) = admitted(self, context, reservation_bytes, || {
            let handles = source_handles(request)?;
            let expires = self.owned_handle_expiry(&handles, context, deadline)?;
            let token = self.owned_token();
            let plans = PageSource {
                metadata,
                field,
                records: Records::Borrowed(records_json),
                usage,
                work,
                token: &token,
                base: 0,
                limit,
            }
            .plans(cancel, deadline)?;
            let mut records = VecDeque::with_capacity(records_json.len());
            for record in records_json {
                cancel.check(deadline)?;
                records.push_back(Box::<str>::from(*record));
            }
            let mut operation = OwnedOperation {
                token,
                claimant: text(context, "claimant")?.to_owned(),
                context_hash: context_hash(context)?,
                handles,
                expires,
                deadline,
                cancel: cancel.clone(),
                metadata: metadata.clone(),
                field: field.to_owned(),
                records,
                usage: usage.clone(),
                work: work.clone(),
                plans,
                next: 0,
                published: 0,
                limit,
                accounted: 0,
            };
            operation.accounted = operation.retained_charge();
            Ok(operation)
        })?;
        let encoded = page(&mut operation, cancel)?;
        self.owned_handle_expiry(&operation.handles, context, deadline)?;
        cancel.check(deadline)?;
        let delivery_cursor = (!operation.plans.is_empty())
            .then(|| format!("{}:{}", operation.token, operation.next));
        let original_cancel = operation.cancel.clone();
        let deadline = operation.deadline;
        if !operation.plans.is_empty() {
            let retained = operation.accounted_bytes();
            let handles = operation.handles.clone();
            self.publish_owned_bound(
                &mut reservation,
                retained,
                &handles,
                context,
                deadline,
                |registry| registry.insert(operation),
            )?;
        }
        if let Err(error) = original_cancel
            .check(deadline)
            .and_then(|_| cancel.check(deadline))
        {
            if let Some(cursor) = &delivery_cursor {
                self.with_owned(|registry| {
                    registry.discard_cursor(cursor, text(context, "claimant")?)
                })?;
            }
            return Err(error);
        }
        self.track_delivery(&sonic_rs::json!({"cursor":delivery_cursor}), context, false);
        Ok(OwnedProjectionReply {
            encoded,
            _reservation: reservation,
        })
    }

    pub fn resume_projection<'a>(
        &'a self,
        cursor: &str,
        context: &Value,
        cancel: &Cancellation,
    ) -> Result<Option<OwnedProjectionReply<'a>>, SnapshotError> {
        if !cursor.starts_with(DOMAIN_PREFIX) {
            return Ok(None);
        }
        let claimant = text(context, "claimant")?;
        if let Err(error) = cancel.check(u64::MAX) {
            self.with_owned(|registry| registry.discard_cursor(cursor, claimant))?;
            return Err(error);
        }
        let extra = checked_add(
            charge(context).checked_mul(3).ok_or_else(output_limit)?,
            MAX_REPLY_BYTES * 6 + 8192,
        )?;
        let mut reservation = self.reserve_projection(context, extra)?;
        let binding = context_hash(context)?;
        let (token, offset) = split_cursor(cursor)?;
        let retained = self.with_owned(|registry| {
            let operation = registry.find(token).ok_or_else(stale)?;
            operation.valid(offset, claimant, &binding, now_ms())?;
            Ok(operation.accounted_bytes())
        })?;
        let mut operation = self.take_owned(&mut reservation, retained, |registry| {
            let operation = registry.find(token).ok_or_else(stale)?;
            operation.valid(offset, claimant, &binding, now_ms())?;
            Ok(registry.take(token).expect("validated operation"))
        })?;
        self.owned_handle_expiry(&operation.handles, context, operation.deadline)?;
        let encoded = page(&mut operation, cancel)?;
        self.owned_handle_expiry(&operation.handles, context, operation.deadline)?;
        cancel.check(operation.deadline)?;
        let delivery_cursor = (!operation.plans.is_empty())
            .then(|| format!("{}:{}", operation.token, operation.next));
        let original_cancel = operation.cancel.clone();
        let deadline = operation.deadline;
        if !operation.plans.is_empty() {
            let retained = operation.accounted_bytes();
            let handles = operation.handles.clone();
            let deadline = operation.deadline;
            self.publish_owned_bound(
                &mut reservation,
                retained,
                &handles,
                context,
                deadline,
                |registry| registry.insert(operation),
            )?;
        }
        if let Err(error) = original_cancel
            .check(deadline)
            .and_then(|_| cancel.check(deadline))
        {
            if let Some(cursor) = &delivery_cursor {
                self.with_owned(|registry| {
                    registry.discard_cursor(cursor, text(context, "claimant")?)
                })?;
            }
            return Err(error);
        }
        self.track_delivery(&sonic_rs::json!({"cursor":delivery_cursor}), context, false);
        Ok(Some(OwnedProjectionReply {
            encoded,
            _reservation: reservation,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot::SCHEMA;
    use sonic_rs::json;
    use std::cell::Cell;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Fixture {
        store: NativeStore,
        context: Value,
        request: Value,
        path: PathBuf,
    }
    impl Fixture {
        fn new() -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let path = std::env::temp_dir().join(format!(
                "snapshot-owned-{}-{}-{}.jsonl",
                std::process::id(),
                now_ms(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::write(&path,b"{\"type\":\"user\",\"uuid\":\"u\",\"sessionId\":\"s\",\"timestamp\":\"2026-01-02T03:04:05Z\",\"message\":{\"content\":\"hello\"}}\n").unwrap();
            let store=NativeStore::new(&json!({"max_retained_bytes":64*1024*1024,"reserved_hook_accounted_bytes":4096,"max_leases":16,"reserved_hook_leases":1})).unwrap();
            let context = json!({"claimant":"owner","admission":"hook","authority":{"kind":"user","effective_uid":unsafe{libc::geteuid()}.to_string()},"registry_generation":store.default_registry_generation()});
            let limits = json!({"max_read_bytes":1024*1024,"max_events":1000,"max_items":1000,"max_output_bytes":MAX_OWNED_BYTES,"max_discovery_entries":1000,"max_sources":100});
            let mut result=store.request(&json!({"schema":SCHEMA,"id":"acquire","operation":"acquire","path":path.to_string_lossy().as_ref(),"classifier":{"id":"native","version":"1"},"deadline_unix_ms":now_ms()+30_000,"limits":limits}),&context,&Cancellation::default());
            for _ in 0..100 {
                if result["status"].as_str() != Some("incomplete") {
                    break;
                }
                result=store.request(&json!({"schema":SCHEMA,"id":"resume","operation":"resume","cursor":result["cursor"]}),&context,&Cancellation::default());
            }
            assert_eq!(result["status"].as_str(), Some("ok"), "{result:?}");
            let request = json!({"id":"publication","deadline_unix_ms":now_ms()+30_000,"limits":limits,"view":{"handle":result["data"]["description"]["handle"]}});
            Self {
                store,
                context,
                request,
                path,
            }
        }

        fn publish<'a>(&'a self, cancel: &Cancellation) -> OwnedProjectionReply<'a> {
            let records: Vec<_> = (0..300)
                .map(|index| format!("{{\"index\":{index}}}"))
                .collect();
            let refs: Vec<_> = records.iter().map(String::as_str).collect();
            self.store.publish_projection(&self.request,&self.context,cancel,&json!({"policy":"one","score":0.25}),"candidates_json",&refs,&json!({"source_opens":2,"output_bytes":7}),&json!({"read_bytes":64,"events":3,"items":300,"output_bytes":5,"elapsed_ms":9})).unwrap()
        }

        fn retained(&self) -> usize {
            self.store
                .with_owned(|registry| Ok(registry.accounted_bytes()))
                .unwrap()
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            std::fs::remove_file(&self.path).unwrap();
        }
    }

    #[test]
    fn two_pages_keep_cumulative_work_and_release_buffers_without_snapshot_pins() {
        let fixture = Fixture::new();
        let snapshot = fixture
            .store
            .pin(&fixture.request["view"]["handle"], &fixture.context)
            .unwrap();
        let pins = std::sync::Arc::strong_count(&snapshot);
        let first = fixture.publish(&Cancellation::default());
        let first_bytes = first.json().len();
        let first_value: Value = sonic_rs::from_str(first.json()).unwrap();
        assert_eq!(first_value["records_json"].as_array().unwrap().len(), 256);
        assert_eq!(first_value["complete"].as_bool(), Some(false));
        assert_eq!(
            first_value["usage"]["output_bytes"].as_u64(),
            Some(first_bytes as u64)
        );
        assert_eq!(
            first_value["work"]["published_output_bytes"].as_u64(),
            Some(first_bytes as u64)
        );
        assert_eq!(first_value["work"]["output_bytes"].as_u64(), Some(5));
        assert_eq!(
            first_value["work"]["produced_output_bytes"].as_u64(),
            Some(7)
        );
        assert_eq!(first_value["work"]["read_bytes"].as_u64(), Some(64));
        assert_eq!(std::sync::Arc::strong_count(&snapshot), pins);
        assert!(fixture.retained() > 0);
        let cursor = first_value["cursor"].as_str().unwrap();
        drop(first);
        let second = fixture
            .store
            .resume_projection(cursor, &fixture.context, &Cancellation::default())
            .unwrap()
            .unwrap();
        let total = first_bytes + second.json().len();
        let second_value: Value = sonic_rs::from_str(second.json()).unwrap();
        assert_eq!(second_value["records_json"].as_array().unwrap().len(), 44);
        assert_eq!(second_value["complete"].as_bool(), Some(true));
        assert!(second_value["cursor"].is_null());
        assert_eq!(
            second_value["usage"]["output_bytes"].as_u64(),
            Some(total as u64)
        );
        assert_eq!(
            second_value["work"]["published_output_bytes"].as_u64(),
            Some(total as u64)
        );
        assert_eq!(second_value["work"]["events"].as_u64(), Some(3));
        assert_eq!(second_value["metadata"]["score"].as_f64(), Some(0.25));
        assert_eq!(fixture.retained(), 0);
        assert_eq!(std::sync::Arc::strong_count(&snapshot), pins);
        assert!(
            matches!(fixture.store.resume_projection(cursor,&fixture.context,&Cancellation::default()),Err(error)if error.status==Status::StaleCursor)
        );
    }

    #[test]
    fn claimant_and_context_changes_cannot_consume_another_cursor() {
        let fixture = Fixture::new();
        let reply = fixture.publish(&Cancellation::default());
        let value: Value = sonic_rs::from_str(reply.json()).unwrap();
        let cursor = value["cursor"].as_str().unwrap();
        let retained = fixture.retained();
        let mut other = fixture.context.clone();
        other.insert("claimant", json!("other"));
        assert!(
            matches!(fixture.store.resume_projection(cursor,&other,&Cancellation::default()),Err(error)if error.status==Status::StaleCursor)
        );
        let mut changed = fixture.context.clone();
        changed.insert("registry_generation", json!("two"));
        assert!(
            matches!(fixture.store.resume_projection(cursor,&changed,&Cancellation::default()),Err(error)if error.status==Status::StaleCursor)
        );
        assert_eq!(fixture.retained(), retained);
        assert!(fixture
            .store
            .resume_projection(
                "ordinary-cursor",
                &fixture.context,
                &Cancellation::default()
            )
            .unwrap()
            .is_none());
        assert!(fixture
            .store
            .resume_projection(
                "domain-projection:unknown:1",
                &fixture.context,
                &Cancellation::default()
            )
            .is_err());
    }

    #[test]
    fn explicit_lease_release_retires_domain_buffers() {
        let fixture = Fixture::new();
        let reply = fixture.publish(&Cancellation::default());
        let value: Value = sonic_rs::from_str(reply.json()).unwrap();
        let cursor = value["cursor"].as_str().unwrap();
        let handle = &fixture.request["view"]["handle"];
        let released=fixture.store.request(&json!({"schema":SCHEMA,"id":"release","operation":"release","kind":"lease","owner_epoch":handle["owner_epoch"],"token":handle["lease_id"]}),&fixture.context,&Cancellation::default());
        assert_eq!(released["status"].as_str(), Some("ok"));
        assert_eq!(fixture.retained(), 0);
        assert!(fixture
            .store
            .resume_projection(cursor, &fixture.context, &Cancellation::default())
            .is_err());
    }

    #[test]
    fn cancellation_discards_only_the_bound_owned_operation() {
        let fixture = Fixture::new();
        let first = fixture.publish(&Cancellation::default());
        let first_value: Value = sonic_rs::from_str(first.json()).unwrap();
        let first_cursor = first_value["cursor"].as_str().unwrap();
        let second = fixture.publish(&Cancellation::default());
        let second_value: Value = sonic_rs::from_str(second.json()).unwrap();
        let second_cursor = second_value["cursor"].as_str().unwrap();
        assert_ne!(first_cursor, second_cursor);
        let cancel = Cancellation::default();
        cancel.cancel();
        assert!(
            matches!(fixture.store.resume_projection(first_cursor,&fixture.context,&cancel),Err(error)if error.status==Status::Cancelled)
        );
        assert!(fixture.retained() > 0);
        assert!(fixture
            .store
            .resume_projection(second_cursor, &fixture.context, &Cancellation::default())
            .unwrap()
            .is_some());
        assert_eq!(fixture.retained(), 0);
    }

    #[test]
    fn expiry_and_deadline_are_not_renewed_by_resume() {
        let fixture = Fixture::new();
        let reply = fixture.publish(&Cancellation::default());
        let value: Value = sonic_rs::from_str(reply.json()).unwrap();
        let cursor = value["cursor"].as_str().unwrap();
        let (token, _) = split_cursor(cursor).unwrap();
        fixture
            .store
            .with_owned(|registry| {
                let mut operation = registry.take(token).unwrap();
                operation.expires = 0;
                registry.insert(operation)
            })
            .unwrap();
        assert!(fixture
            .store
            .resume_projection(cursor, &fixture.context, &Cancellation::default())
            .is_err());
        assert_eq!(fixture.retained(), 0);
        let reply = fixture.publish(&Cancellation::default());
        let value: Value = sonic_rs::from_str(reply.json()).unwrap();
        let cursor = value["cursor"].as_str().unwrap();
        let (token, _) = split_cursor(cursor).unwrap();
        fixture
            .store
            .with_owned(|registry| {
                let mut operation = registry.take(token).unwrap();
                operation.deadline = 0;
                registry.insert(operation)
            })
            .unwrap();
        assert!(fixture
            .store
            .resume_projection(cursor, &fixture.context, &Cancellation::default())
            .is_err());
        assert_eq!(fixture.retained(), 0);
    }

    #[test]
    fn operation_limit_is_cumulative_and_checked_before_retained_copy() {
        let fixture = Fixture::new();
        let huge = "x".repeat(MAX_OWNED_BYTES / 2 + 1);
        let records = [huge.as_str(), huge.as_str()];
        assert!(
            matches!(fixture.store.publish_projection(&fixture.request,&fixture.context,&Cancellation::default(),&json!({}),"records",&records,&json!({"output_bytes":0}),&json!({"read_bytes":0,"events":0,"items":0,"output_bytes":0})),Err(error)if error.status==Status::OutputLimit)
        );
        assert_eq!(fixture.retained(), 0);
        let mut request = fixture.request.clone();
        request["limits"].insert("max_output_bytes", json!(800));
        let records = vec!["{}"; 300];
        assert!(
            matches!(fixture.store.publish_projection(&request,&fixture.context,&Cancellation::default(),&json!({}),"records",&records,&json!({"output_bytes":0}),&json!({"read_bytes":0,"events":0,"items":0,"output_bytes":0})),Err(error)if error.status==Status::OutputLimit)
        );
        assert_eq!(fixture.retained(), 0);
    }

    #[test]
    fn retained_admission_precedes_copy_callback_and_uses_global_pool() {
        let cap: usize = 4096;
        let store =
            NativeStore::new(&json!({"max_retained_bytes":cap,"reserved_hook_accounted_bytes":0}))
                .unwrap();
        let context = json!({"claimant":"owner","admission":"hook","authority":{"kind":"user","effective_uid":unsafe{libc::geteuid()}.to_string()},"registry_generation":store.default_registry_generation()});
        let accounted = || store.retained_accounted_bytes();
        let baseline = accounted();
        assert!(baseline > 0);
        let remaining = cap.checked_sub(baseline).unwrap();
        assert!(remaining >= 2);
        let first = remaining / 2 + 1;
        let second = remaining - first + 1;
        let copies = Cell::new(0);
        let result = admitted(&store, &context, remaining + 1, || {
            copies.set(copies.get() + 1);
            Ok(vec!["copied".to_owned()])
        });
        assert!(matches!(result,Err(error)if error.status==Status::RetainedLimit));
        assert_eq!(copies.get(), 0);
        let guard = store
            .reserve_owned_input(&context, &Cancellation::default(), first)
            .unwrap();
        assert_eq!(accounted(), baseline + first);
        assert!(
            matches!(store.reserve_owned_input(&context,&Cancellation::default(),second),Err(error)if error.status==Status::RetainedLimit)
        );
        assert_eq!(accounted(), baseline + first);
        drop(guard);
        assert_eq!(accounted(), baseline);
        let guard = store
            .reserve_owned_input(&context, &Cancellation::default(), second)
            .unwrap();
        assert_eq!(accounted(), baseline + second);
        drop(guard);
        assert_eq!(accounted(), baseline);
    }

    #[test]
    fn failed_transport_discard_releases_only_matching_cursor() {
        let fixture = Fixture::new();
        let reply = fixture.publish(&Cancellation::default());
        let value: Value = sonic_rs::from_str(reply.json()).unwrap();
        assert!(fixture
            .store
            .discard_response(&value, &fixture.context)
            .unwrap());
        assert_eq!(fixture.retained(), 0);
        assert!(!fixture
            .store
            .discard_response(&value, &fixture.context)
            .unwrap());
    }
}
