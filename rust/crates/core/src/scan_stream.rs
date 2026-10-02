#[cfg(test)]
use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::sync::{LazyLock, Mutex};

use crate::scan::{ScanBudget, StagingReservation};
use crate::scan_checkpoint::Prefix;
use crate::snapshot::{Cancellation, SnapshotError, SourceStamp, Status};

const READ_BLOCK: usize = 64 * 1024;
const VALIDATE_BLOCK: usize = 1024 * 1024;

#[cfg(test)]
type ValidateHook = Box<dyn FnMut(u64) + Send>;

#[cfg(test)]
pub(crate) static VALIDATE_HOOKS: LazyLock<Mutex<HashMap<PathBuf, ValidateHook>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

#[cfg(test)]
fn run_validate_hook(path: &Path, validated: u64) {
    if let Some(hook) = VALIDATE_HOOKS.lock().expect("validate hooks").get_mut(path) {
        hook(validated);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LineSpan {
    pub offset: u64,
    pub len: usize,
    pub terminated: bool,
}

impl LineSpan {
    pub fn end(&self) -> u64 {
        self.offset + self.len as u64 + u64::from(self.terminated)
    }
}

pub struct SourceStream<'store> {
    pub path: PathBuf,
    pub stamp: SourceStamp,
    file: File,
    offset: u64,
    base: u64,
    consumed: usize,
    buffer: Vec<u8>,
    staging: StagingReservation<'store>,
}

fn changed(reason: &str) -> SnapshotError {
    SnapshotError::new(Status::Changed, reason)
}

fn io_error(error: std::io::Error) -> SnapshotError {
    SnapshotError::new(Status::Incomplete, error.to_string())
}

impl<'store> SourceStream<'store> {
    pub fn open(
        path: &Path,
        start: u64,
        budget: &mut ScanBudget<'store>,
        cancel: &Cancellation,
    ) -> Result<Self, SnapshotError> {
        budget.checkpoint(cancel)?;
        let mut file = File::open(path).map_err(io_error)?;
        let metadata = file.metadata().map_err(io_error)?;
        if !metadata.is_file() {
            return Err(SnapshotError::new(
                Status::InvalidRequest,
                "source must be a regular file",
            ));
        }
        budget.progress.source_opens += 1;
        let stamp = SourceStamp::of(&metadata);
        if start > stamp.size {
            return Err(changed("source shrank below the resume offset"));
        }
        file.seek(SeekFrom::Start(start)).map_err(io_error)?;
        Ok(Self {
            path: path.to_owned(),
            stamp,
            file,
            offset: start,
            base: start,
            consumed: 0,
            buffer: Vec::new(),
            staging: budget.reserve_staging(0, cancel)?,
        })
    }

    pub fn generation(&self) -> String {
        format!(
            "stream:{}:{}:{}",
            self.stamp.identity.device, self.stamp.identity.inode, self.stamp.size
        )
    }

    pub fn bytes(&self, line: &LineSpan) -> &[u8] {
        let start = (line.offset - self.base) as usize;
        &self.buffer[start..start + line.len]
    }

    pub fn next_line(
        &mut self,
        budget: &mut ScanBudget<'store>,
        cancel: &Cancellation,
    ) -> Result<Option<LineSpan>, SnapshotError> {
        loop {
            budget.checkpoint(cancel)?;
            if let Some(at) = memchr::memchr(b'\n', &self.buffer[self.consumed..]) {
                let line = LineSpan {
                    offset: self.base + self.consumed as u64,
                    len: at,
                    terminated: true,
                };
                self.consumed += at + 1;
                return Ok(Some(line));
            }
            if self.offset == self.stamp.size {
                if self.consumed == self.buffer.len() {
                    return Ok(None);
                }
                let line = LineSpan {
                    offset: self.base + self.consumed as u64,
                    len: self.buffer.len() - self.consumed,
                    terminated: false,
                };
                self.consumed = self.buffer.len();
                return Ok(Some(line));
            }
            self.fill(budget, cancel)?;
        }
    }

    fn fill(
        &mut self,
        budget: &mut ScanBudget<'store>,
        cancel: &Cancellation,
    ) -> Result<(), SnapshotError> {
        self.buffer.drain(..self.consumed);
        self.base += self.consumed as u64;
        self.consumed = 0;
        let count = READ_BLOCK
            .min((self.stamp.size - self.offset) as usize)
            .min(budget.remaining().max_source_read_bytes);
        if count == 0 {
            return Err(budget.read_exhausted());
        }
        let needed = self.buffer.len() + count;
        if needed > self.buffer.capacity() {
            let capacity = needed.next_power_of_two();
            budget.extend_staging(&mut self.staging, capacity - self.buffer.capacity(), cancel)?;
            self.buffer.reserve_exact(capacity - self.buffer.len());
        }
        let start = self.buffer.len();
        self.buffer.resize(start + count, 0);
        self.file
            .read_exact(&mut self.buffer[start..])
            .map_err(|_| changed("source changed while reading"))?;
        budget.charge_source(count)?;
        self.offset += count as u64;
        Ok(())
    }

    pub fn seek(&mut self, offset: u64) -> Result<(), SnapshotError> {
        self.file.seek(SeekFrom::Start(offset)).map_err(io_error)?;
        self.buffer.clear();
        self.consumed = 0;
        self.base = offset;
        self.offset = offset;
        Ok(())
    }

    pub fn read_span(
        &mut self,
        line: &LineSpan,
        budget: &mut ScanBudget<'store>,
        cancel: &Cancellation,
    ) -> Result<Vec<u8>, SnapshotError> {
        budget.charge_source(line.len)?;
        self.read_at(line, budget, cancel)
    }

    pub fn revalidate_span(
        &mut self,
        line: &LineSpan,
        budget: &mut ScanBudget<'store>,
        cancel: &Cancellation,
    ) -> Result<Vec<u8>, SnapshotError> {
        let bytes = self.read_at(line, budget, cancel)?;
        budget.charge_validation(bytes.len());
        Ok(bytes)
    }

    fn read_at(
        &mut self,
        line: &LineSpan,
        budget: &mut ScanBudget<'store>,
        cancel: &Cancellation,
    ) -> Result<Vec<u8>, SnapshotError> {
        budget.checkpoint(cancel)?;
        if line.end() > self.stamp.size {
            return Err(changed("source shrank below a recorded line"));
        }
        let _staging = budget.reserve_staging(line.len, cancel)?;
        let mut bytes = vec![0; line.len];
        self.file
            .seek(SeekFrom::Start(line.offset))
            .and_then(|_| self.file.read_exact(&mut bytes))
            .and_then(|()| self.file.seek(SeekFrom::Start(self.offset)))
            .map_err(|_| changed("source changed while reading"))?;
        Ok(bytes)
    }

    pub fn validate_prefix(
        &mut self,
        segments: &[(u64, u64)],
        span: u64,
        budget: &mut ScanBudget<'store>,
        cancel: &Cancellation,
    ) -> Result<Option<Prefix>, SnapshotError> {
        let end = segments.last().map_or(0, |(end, _)| *end);
        let block = VALIDATE_BLOCK.min(end as usize);
        let _staging = budget.reserve_staging(block, cancel)?;
        let mut buffer = vec![0; block];
        self.file.seek(SeekFrom::Start(0)).map_err(io_error)?;
        let mut prefix = Prefix::new(span);
        while prefix.end() < end && prefix.agrees(segments) {
            budget.checkpoint(cancel)?;
            let count = block
                .min((end - prefix.end()) as usize)
                .min(prefix.room() as usize);
            self.file
                .read_exact(&mut buffer[..count])
                .map_err(|_| changed("source changed while validating"))?;
            budget.charge_validation(count);
            prefix.write(&buffer[..count]);
            #[cfg(test)]
            run_validate_hook(&self.path, prefix.end());
        }
        self.file
            .seek(SeekFrom::Start(self.offset))
            .map_err(io_error)?;
        Ok((prefix.segments() == segments).then_some(prefix))
    }

    pub fn observe(&self) -> Result<[SourceStamp; 2], SnapshotError> {
        Ok([
            SourceStamp::of(&self.file.metadata().map_err(io_error)?),
            SourceStamp::of(&std::fs::metadata(&self.path).map_err(io_error)?),
        ])
    }

    pub fn verify(&self) -> Result<Option<[SourceStamp; 2]>, SnapshotError> {
        let observed = self.observe()?;
        if observed.iter().any(|current| {
            *current != self.stamp
                && (current.identity != self.stamp.identity || current.size <= self.stamp.size)
        }) {
            return Err(changed("source changed during scan"));
        }
        Ok(observed
            .iter()
            .any(|current| *current != self.stamp)
            .then_some(observed))
    }
}
