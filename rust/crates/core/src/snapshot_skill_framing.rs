use crate::snapshot_ledger::vec_growth;

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct EntryTooLong(pub usize);

#[derive(Debug)]
enum ProviderLine {
    Searching(Vec<u8>),
    Ready(Vec<u8>),
    Classified,
}

#[derive(Debug)]
pub(crate) struct SkillFraming {
    first: ProviderLine,
    line_bytes: usize,
    max_line_bytes: usize,
    first_line_committed: bool,
    fence: Vec<u8>,
}

impl SkillFraming {
    pub(crate) fn new() -> Self {
        Self {
            first: ProviderLine::Searching(Vec::new()),
            line_bytes: 0,
            max_line_bytes: 0,
            first_line_committed: false,
            fence: Vec::new(),
        }
    }

    pub(crate) fn after_prefix(max_line_bytes: usize, tail_bytes: usize, fence: Vec<u8>) -> Self {
        Self {
            first: ProviderLine::Classified,
            line_bytes: tail_bytes,
            max_line_bytes,
            first_line_committed: true,
            fence,
        }
    }

    pub(crate) fn growth(&self, bytes: usize) -> usize {
        let first = match &self.first {
            ProviderLine::Searching(line) => vec_growth(line, bytes),
            ProviderLine::Ready(_) | ProviderLine::Classified => 0,
        };
        first
            + vec_growth(
                &self.fence,
                (self.fence.len() + bytes)
                    .min(64)
                    .saturating_sub(self.fence.len()),
            )
    }

    pub(crate) fn push(&mut self, bytes: &[u8], entry_bound: usize) -> Result<(), EntryTooLong> {
        for part in bytes.split_inclusive(|byte| *byte == b'\n') {
            let newline = part.last() == Some(&b'\n');
            let line = if newline {
                &part[..part.len() - 1]
            } else {
                part
            };
            self.line_bytes += line.len();
            if self.line_bytes > entry_bound {
                return Err(EntryTooLong(self.line_bytes));
            }
            self.max_line_bytes = self.max_line_bytes.max(self.line_bytes);
            if let ProviderLine::Searching(first) = &mut self.first {
                first.extend_from_slice(line);
                if newline {
                    if first.iter().all(u8::is_ascii_whitespace) {
                        first.clear();
                    } else {
                        self.first = ProviderLine::Ready(std::mem::take(first));
                        self.first_line_committed = true;
                    }
                }
            }
            if newline {
                self.line_bytes = 0;
            }
        }
        if bytes.len() >= 64 {
            self.fence.clear();
            self.fence.extend_from_slice(&bytes[bytes.len() - 64..]);
        } else {
            let removed = (self.fence.len() + bytes.len()).saturating_sub(64);
            self.fence.drain(..removed);
            self.fence.extend_from_slice(bytes);
        }
        Ok(())
    }

    pub(crate) fn finish(&mut self) {
        if let ProviderLine::Searching(first) = &mut self.first {
            self.first = ProviderLine::Ready(std::mem::take(first));
        }
    }

    pub(crate) fn provider_line(&self) -> Option<&[u8]> {
        match &self.first {
            ProviderLine::Ready(line) => Some(line),
            ProviderLine::Searching(_) | ProviderLine::Classified => None,
        }
    }

    pub(crate) fn classified(&mut self) {
        self.first = ProviderLine::Classified;
    }

    pub(crate) fn accounted_bytes(&self) -> usize {
        self.fence.capacity()
            + match &self.first {
                ProviderLine::Searching(line) | ProviderLine::Ready(line) => line.capacity(),
                ProviderLine::Classified => 0,
            }
    }

    #[cfg(test)]
    pub(crate) fn audit_bytes(&self) -> usize {
        let first = match &self.first {
            ProviderLine::Searching(bytes) | ProviderLine::Ready(bytes) => bytes.capacity(),
            ProviderLine::Classified => 0,
        };
        first + self.fence.capacity()
    }

    pub(crate) fn max_line_bytes(&self) -> usize {
        self.max_line_bytes
    }

    pub(crate) fn tail_bytes(&self) -> usize {
        self.line_bytes
    }

    pub(crate) fn first_line_committed(&self) -> bool {
        self.first_line_committed
    }

    pub(crate) fn fence(&self) -> &[u8] {
        &self.fence
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typed_skills_framing_provider_and_all_later_lines_across_chunks() {
        let bytes = b" \t\n{\"type\":\"session_meta\",\"payload\":{}}\n1234567\nxyz";
        for step in 1..=bytes.len() {
            let mut framing = SkillFraming::new();
            for chunk in bytes.chunks(step) {
                framing.push(chunk, 128).unwrap();
            }
            framing.finish();
            assert_eq!(
                framing.provider_line(),
                Some(b"{\"type\":\"session_meta\",\"payload\":{}}".as_slice())
            );
            assert!(framing.first_line_committed());
            assert_eq!(framing.max_line_bytes(), 36);
            assert_eq!(framing.tail_bytes(), 3);
            assert_eq!(framing.fence(), &bytes[bytes.len().saturating_sub(64)..]);
        }
    }

    #[test]
    fn typed_skills_framing_rejects_long_lines_and_unterminated_tail() {
        for suffix in [b"123456789\n".as_slice(), b"123456789".as_slice()] {
            let mut framing = SkillFraming::new();
            framing.push(b"meta\n", 8).unwrap();
            framing.classified();
            assert_eq!(framing.push(suffix, 8), Err(EntryTooLong(9)));
        }
    }

    #[test]
    fn typed_skills_framing_append_carries_line_and_fence() {
        let mut framing = SkillFraming::after_prefix(7, 3, b"header\n123".to_vec());
        framing.push(b"4567\nnext", 7).unwrap();
        assert_eq!(framing.max_line_bytes(), 7);
        assert_eq!(framing.tail_bytes(), 4);
        assert_eq!(framing.fence(), b"header\n1234567\nnext");
        assert!(framing.provider_line().is_none());
    }

    #[test]
    fn typed_skills_framing_provisional_header_is_not_an_append_certificate() {
        let mut framing = SkillFraming::new();
        framing.push(b"meta", 8).unwrap();
        assert!(framing.provider_line().is_none());
        framing.finish();
        assert_eq!(framing.provider_line(), Some(b"meta".as_slice()));
        assert!(!framing.first_line_committed());
    }
}
