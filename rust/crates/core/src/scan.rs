use std::path::{Path, PathBuf};

use serde::Serialize;
use sonic_rs::{json, JsonContainerTrait, JsonValueTrait, Value};

use crate::snapshot::{
    Cancellation, NativeStore, SnapshotError, Status, TranscriptSnapshot, WorkLimits, SCHEMA,
};

#[derive(Debug, Default, Clone, Serialize)]
pub struct ScanProgress {
    pub discovery_entries: usize,
    pub sources: usize,
    pub source_bytes: usize,
    pub parsed_events: usize,
    pub preparation_reserved_events: usize,
    pub examined_events: usize,
    pub projection_bytes: usize,
    pub output_bytes: usize,
    pub source_opens: usize,
    pub cache_hits: usize,
}

pub struct ScanBudget {
    pub limits: WorkLimits,
    pub progress: ScanProgress,
}

impl ScanBudget {
    pub fn new(limits: WorkLimits) -> Self {
        Self {
            limits,
            progress: ScanProgress::default(),
        }
    }

    pub fn checkpoint(&self, cancel: &Cancellation) -> Result<(), SnapshotError> {
        cancel.check(self.limits.deadline_unix_ms)
    }

    pub fn remaining(&self) -> WorkLimits {
        WorkLimits {
            max_read_bytes: self
                .limits
                .max_read_bytes
                .saturating_sub(self.progress.source_bytes)
                .saturating_sub(self.progress.projection_bytes),
            max_events: self
                .limits
                .max_events
                .saturating_sub(self.progress.parsed_events)
                .saturating_sub(self.progress.preparation_reserved_events)
                .saturating_sub(self.progress.examined_events),
            max_output_bytes: self
                .limits
                .max_output_bytes
                .saturating_sub(self.progress.output_bytes),
            max_discovery_entries: self
                .limits
                .max_discovery_entries
                .saturating_sub(self.progress.discovery_entries),
            max_sources: self
                .limits
                .max_sources
                .saturating_sub(self.progress.sources),
            ..self.limits
        }
    }

    pub fn charge_projection(
        &mut self,
        bytes: usize,
        events: usize,
        cancel: &Cancellation,
    ) -> Result<(), SnapshotError> {
        self.checkpoint(cancel)?;
        let remaining = self.remaining();
        if bytes > remaining.max_read_bytes || events > remaining.max_events {
            return Err(incomplete("projection work budget exhausted"));
        }
        self.progress.projection_bytes += bytes;
        self.progress.examined_events += events;
        Ok(())
    }

    pub fn charge_output(
        &mut self,
        bytes: usize,
        cancel: &Cancellation,
    ) -> Result<(), SnapshotError> {
        self.checkpoint(cancel)?;
        if bytes > self.remaining().max_output_bytes {
            return Err(SnapshotError::new(
                Status::OutputLimit,
                "scan output budget exhausted",
            ));
        }
        self.progress.output_bytes += bytes;
        Ok(())
    }

    fn account(&mut self, response: &Value) -> Result<(), SnapshotError> {
        let usage = &response["usage"];
        self.progress.source_bytes = self
            .progress
            .source_bytes
            .saturating_add(count(usage, "source_bytes_read")?);
        self.progress.parsed_events = self
            .progress
            .parsed_events
            .saturating_add(count(usage, "events_parsed")?);
        self.progress.source_opens = self
            .progress
            .source_opens
            .saturating_add(count(usage, "source_opens")?);
        self.progress.cache_hits = self
            .progress
            .cache_hits
            .saturating_add(count(usage, "cache_hits")?);
        self.progress.discovery_entries = self
            .progress
            .discovery_entries
            .saturating_add(count(usage, "discovery_entries_examined")?);
        if self
            .progress
            .source_bytes
            .saturating_add(self.progress.projection_bytes)
            > self.limits.max_read_bytes
            || self
                .progress
                .parsed_events
                .saturating_add(self.progress.preparation_reserved_events)
                .saturating_add(self.progress.examined_events)
                > self.limits.max_events
            || self.progress.discovery_entries > self.limits.max_discovery_entries
        {
            return Err(incomplete("cumulative scan work budget exhausted"));
        }
        Ok(())
    }
}

pub struct ScanPlan {
    pub paths: Vec<PathBuf>,
    pub root: PathBuf,
    pub project: Option<String>,
    pub contains: Option<String>,
    pub source_limit: Option<usize>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ScanOutcome {
    pub complete: bool,
    pub reason: Option<String>,
    pub selected_sources: usize,
    pub available_sources: usize,
    pub progress: ScanProgress,
}

pub enum ScanControl {
    Continue,
    Stop { source_complete: bool },
}

pub struct ScanSession<'a> {
    store: &'a NativeStore,
    context: Value,
    pub budget: ScanBudget,
    cancel: Cancellation,
    sequence: usize,
    pending: Option<Value>,
}

struct ResponseGuard<'a> {
    store: &'a NativeStore,
    context: &'a Value,
    response: Value,
    keep: bool,
}

impl Drop for ResponseGuard<'_> {
    fn drop(&mut self) {
        if self.keep {
            return;
        }
        let _ = self.store.discard_response(&self.response, self.context);
        if let Some(token) = self.response["data"]["checkpoint"].as_str() {
            self.store.request(&json!({"schema":SCHEMA,"id":"scan-cleanup","operation":"release","kind":"cursor","token":token}), self.context, &Cancellation::default());
        }
    }
}

impl<'a> ScanSession<'a> {
    pub fn new(store: &'a NativeStore, limits: WorkLimits, cancel: Cancellation) -> Self {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let context = json!({"claimant":format!("cli-scan-{}-{}",std::process::id(),NEXT.fetch_add(1,std::sync::atomic::Ordering::Relaxed)),"admission":"background","authority":{"kind":"user","effective_uid":unsafe { libc::geteuid() }.to_string()},"registry_generation":store.default_registry_generation()});
        Self {
            store,
            context,
            budget: ScanBudget::new(limits),
            cancel,
            sequence: 0,
            pending: None,
        }
    }

    fn request(&mut self, mut request: Value) -> Result<Value, SnapshotError> {
        self.budget.checkpoint(&self.cancel)?;
        self.sequence += 1;
        request.insert("schema", json!(SCHEMA));
        request.insert("id", json!(format!("scan-{}", self.sequence)));
        let response = self.store.request(&request, &self.context, &self.cancel);
        if let Err(error) = self.budget.account(&response) {
            self.store.discard_response(&response, &self.context)?;
            return Err(error);
        }
        self.pending = response["cursor"].as_str().map(|_| response.clone());
        Ok(response)
    }

    fn bounded_request(&self, operation: &str) -> Value {
        let limits = self.budget.remaining();
        json!({"operation":operation,"deadline_unix_ms":limits.deadline_unix_ms,"limits":{"max_read_bytes":limits.max_read_bytes,"max_events":limits.max_events,"max_items":limits.max_items,"max_output_bytes":limits.max_output_bytes,"max_discovery_entries":limits.max_discovery_entries,"max_sources":limits.max_sources}})
    }

    fn targets(&mut self, plan: &ScanPlan) -> Result<(Vec<PathBuf>, usize), SnapshotError> {
        if !plan.paths.is_empty() {
            if plan.paths.len() > self.budget.remaining().max_sources {
                return Err(incomplete("explicit source count exceeds scan budget"));
            }
            let total = plan.paths.len();
            return Ok((plan.paths.clone(), total));
        }
        let mut request = self.bounded_request("discover");
        request.insert("roots", json!([plan.root.to_string_lossy().as_ref()]));
        request.insert("preserve_aliases", json!(true));
        let mut paths = Vec::new();
        loop {
            let response = self.request(request)?;
            let mut guarded = ResponseGuard {
                store: self.store,
                context: &self.context,
                response,
                keep: false,
            };
            let entries = guarded.response["data"]["entries"]
                .as_array()
                .ok_or_else(|| response_error(&guarded.response))?;
            for entry in entries {
                let path = PathBuf::from(string(entry, "path")?);
                let timestamp = string(entry, "mtime_ns")?
                    .parse::<i128>()
                    .map_err(|_| invalid("invalid discovery timestamp"))?;
                if matches_source(&path, plan) {
                    paths.push((
                        path,
                        timestamp.div_euclid(1_000_000_000) as f64
                            + timestamp.rem_euclid(1_000_000_000) as f64 / 1_000_000_000.0,
                    ));
                }
            }
            if let Some(cursor) = guarded.response["cursor"].as_str() {
                request = json!({"operation":"resume","cursor":cursor});
                guarded.keep = true;
                continue;
            }
            if guarded.response["complete"].as_bool() != Some(true) {
                return Err(response_error(&guarded.response));
            }
            break;
        }
        paths.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .expect("finite discovery timestamp")
                .then_with(|| {
                    a.0.as_os_str()
                        .as_encoded_bytes()
                        .cmp(b.0.as_os_str().as_encoded_bytes())
                })
        });
        let total = paths.len();
        let selected = plan.source_limit.unwrap_or(total).min(total);
        paths.truncate(selected);
        Ok((paths.into_iter().map(|(path, _)| path).collect(), total))
    }

    pub fn run<F>(&mut self, plan: &ScanPlan, mut visit: F) -> ScanOutcome
    where
        F: FnMut(
            &Path,
            &TranscriptSnapshot,
            &mut ScanBudget,
            &Cancellation,
        ) -> Result<ScanControl, SnapshotError>,
    {
        let mut selected_sources = 0;
        let mut available_sources = 0;
        let result = (|| {
            let (paths, total) = self.targets(plan)?;
            selected_sources = paths.len();
            available_sources = total;
            for (source_index, path) in paths.into_iter().enumerate() {
                if self.budget.remaining().max_sources == 0 {
                    return Err(incomplete("source budget exhausted"));
                }
                self.budget.progress.sources += 1;
                let mut request = self.bounded_request("acquire");
                request.insert("path", json!(path.to_string_lossy().as_ref()));
                request.insert("classifier", json!({"id":"native","version":"1"}));
                loop {
                    let response = self.request(request)?;
                    if let Some(cursor) = response["cursor"].as_str() {
                        request = json!({"operation":"resume","cursor":cursor});
                        if let Err(error) = self.budget.checkpoint(&self.cancel) {
                            self.store.discard_response(&response, &self.context)?;
                            return Err(error);
                        }
                        continue;
                    }
                    let guarded = ResponseGuard {
                        store: self.store,
                        context: &self.context,
                        response,
                        keep: false,
                    };
                    if guarded.response["status"].as_str() != Some("ok") {
                        return Err(response_error(&guarded.response));
                    }
                    let snapshot = self
                        .store
                        .pin_scope(
                            &guarded.response["data"]["description"]["handle"],
                            &self.context,
                        )?
                        .0;
                    if snapshot.event_count > self.budget.remaining().max_events {
                        return Err(incomplete("preparation event budget exhausted"));
                    }
                    self.budget.progress.preparation_reserved_events += snapshot.event_count;
                    match visit(&path, &snapshot, &mut self.budget, &self.cancel)? {
                        ScanControl::Continue => {}
                        ScanControl::Stop { source_complete } => {
                            return Ok(source_complete && source_index + 1 == selected_sources)
                        }
                    }
                    break;
                }
            }
            Ok(true)
        })();
        let (complete, reason) = match result {
            Ok(true) => (true, None),
            Ok(false) => (false, Some("result_limit".to_owned())),
            Err(error) => (
                false,
                Some(format!("{}: {}", error.status.as_str(), error.reason)),
            ),
        };
        ScanOutcome {
            complete,
            reason,
            selected_sources,
            available_sources,
            progress: self.budget.progress.clone(),
        }
    }
}

impl Drop for ScanSession<'_> {
    fn drop(&mut self) {
        if let Some(response) = self.pending.take() {
            let _ = self.store.discard_response(&response, &self.context);
        }
    }
}

fn matches_source(path: &Path, plan: &ScanPlan) -> bool {
    let filename = path.file_name().unwrap_or_default().to_string_lossy();
    if plan
        .contains
        .as_ref()
        .is_some_and(|part| !filename.contains(part))
    {
        return false;
    }
    plan.project.as_ref().is_none_or(|project| {
        path.strip_prefix(&plan.root)
            .ok()
            .and_then(Path::parent)
            .is_some_and(|parent| {
                parent
                    .components()
                    .any(|part| part.as_os_str().to_string_lossy().contains(project))
            })
    })
}

fn count(value: &Value, key: &str) -> Result<usize, SnapshotError> {
    value
        .get(key)
        .and_then(Value::as_u64)
        .and_then(|n| n.try_into().ok())
        .ok_or_else(|| invalid(format!("missing scan count {key}")))
}

fn string<'a>(value: &'a Value, key: &str) -> Result<&'a str, SnapshotError> {
    value
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| invalid(format!("missing scan string {key}")))
}

fn response_error(response: &Value) -> SnapshotError {
    incomplete(format!(
        "{}: {}",
        response["status"].as_str().unwrap_or("invalid_response"),
        response["reason"]
            .as_str()
            .unwrap_or("missing scan response data")
    ))
}

fn invalid(reason: impl Into<String>) -> SnapshotError {
    SnapshotError::new(Status::InvalidRequest, reason)
}
fn incomplete(reason: impl Into<String>) -> SnapshotError {
    SnapshotError::new(Status::Incomplete, reason)
}

pub fn scan_corpus<F>(
    path: &Path,
    budget: &mut ScanBudget,
    cancel: &Cancellation,
    mut visit: F,
) -> ScanOutcome
where
    F: FnMut(usize, &str, &mut ScanBudget, &Cancellation) -> Result<bool, SnapshotError>,
{
    use std::io::{BufRead, Read};
    let result = (|| {
        budget.checkpoint(cancel)?;
        if budget.remaining().max_sources == 0 {
            return Err(incomplete("source budget exhausted"));
        }
        let file = std::fs::File::open(path).map_err(|e| incomplete(e.to_string()))?;
        let before = file.metadata().map_err(|e| incomplete(e.to_string()))?;
        let read_limit = budget.remaining().max_read_bytes;
        let source_start = budget.progress.source_bytes;
        let read_count = std::rc::Rc::new(std::cell::Cell::new(0));
        let counted = CountedRead {
            inner: file.take(read_limit as u64),
            bytes: read_count.clone(),
        };
        let mut reader = std::io::BufReader::with_capacity(8192.min(read_limit.max(1)), counted);
        budget.progress.sources += 1;
        budget.progress.source_opens += 1;
        let mut line = Vec::new();
        let mut line_number = 0;
        loop {
            budget.checkpoint(cancel)?;
            if reader.buffer().is_empty() {
                reader
                    .get_mut()
                    .inner
                    .set_limit(budget.remaining().max_read_bytes as u64);
            }
            let part = reader.fill_buf().map_err(|e| incomplete(e.to_string()))?;
            budget.progress.source_bytes = source_start.saturating_add(read_count.get());
            if part.is_empty() {
                if read_count.get() < before.len() as usize {
                    return Err(incomplete("corpus read budget exhausted"));
                }
                if !line.is_empty() {
                    line_number += 1;
                    let text = std::str::from_utf8(&line).map_err(|e| incomplete(e.to_string()))?;
                    budget.charge_projection(0, 1, cancel)?;
                    if visit(line_number, text, budget, cancel)? {
                        return Ok(false);
                    }
                }
                let after = reader
                    .get_ref()
                    .inner
                    .get_ref()
                    .metadata()
                    .map_err(|e| incomplete(e.to_string()))?;
                if crate::snapshot::SourceStamp::of(&before)
                    != crate::snapshot::SourceStamp::of(&after)
                {
                    return Err(incomplete("corpus changed during scan"));
                }
                return Ok(true);
            }
            let take = part
                .iter()
                .position(|byte| *byte == b'\n')
                .map_or(part.len(), |at| at + 1);
            if line.len().saturating_add(take) > read_limit {
                return Err(incomplete("corpus line budget exhausted"));
            }
            let complete = part[take - 1] == b'\n';
            line.extend_from_slice(&part[..take]);
            reader.consume(take);
            if complete {
                line_number += 1;
                line.pop();
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
                let text = std::str::from_utf8(&line).map_err(|e| incomplete(e.to_string()))?;
                budget.charge_projection(0, 1, cancel)?;
                if visit(line_number, text, budget, cancel)? {
                    return Ok(false);
                }
                line.clear();
            }
        }
    })();
    let (complete, reason) = match result {
        Ok(true) => (true, None),
        Ok(false) => (false, Some("result_limit".to_owned())),
        Err(error) => (
            false,
            Some(format!("{}: {}", error.status.as_str(), error.reason)),
        ),
    };
    ScanOutcome {
        complete,
        reason,
        selected_sources: 1,
        available_sources: 1,
        progress: budget.progress.clone(),
    }
}

struct CountedRead<R> {
    inner: R,
    bytes: std::rc::Rc<std::cell::Cell<usize>>,
}

impl<R: std::io::Read> std::io::Read for CountedRead<R> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let read = self.inner.read(buffer)?;
        self.bytes.set(self.bytes.get().saturating_add(read));
        Ok(read)
    }
}

#[cfg(test)]
#[path = "scan_regressions.rs"]
mod regression_tests;
