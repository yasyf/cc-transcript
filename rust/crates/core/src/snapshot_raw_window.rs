use std::collections::VecDeque;
use std::ops::Range;

#[derive(Clone)]
struct MatchWindow {
    hit: usize,
    lines: Range<usize>,
}

pub(crate) fn selected_ranges(
    line_count: usize,
    hits: impl IntoIterator<Item = usize>,
    around: usize,
    limit: usize,
) -> Vec<Range<usize>> {
    if limit == 0 {
        return Vec::new();
    }
    let mut earliest = Vec::new();
    let mut newest = VecDeque::new();
    let mut pending: Option<MatchWindow> = None;
    let mut retain = |window: MatchWindow| {
        if earliest.len() < limit {
            earliest.push(window.clone());
        }
        if newest.len() == limit {
            newest.pop_front();
        }
        newest.push_back(window);
    };
    for hit in hits {
        assert!(hit < line_count);
        let previous_end = pending.as_ref().map_or(0, |window| {
            assert!(window.hit < hit);
            window.hit.saturating_add(around).saturating_add(1)
        });
        if let Some(mut previous) = pending.take() {
            previous.lines.end = previous_end.min(hit);
            retain(previous);
        }
        pending = Some(MatchWindow {
            hit,
            lines: hit.saturating_sub(around).max(previous_end.min(hit))..line_count,
        });
    }
    if let Some(mut previous) = pending {
        previous.lines.end = previous.hit.saturating_add(around).saturating_add(1).min(line_count);
        retain(previous);
    }
    let first_end = earliest.last().map(|window| window.hit);
    earliest.into_iter().chain(newest.into_iter().filter(|window| {
        first_end.is_none_or(|last| window.hit > last)
    })).map(|window| window.lines).collect()
}

pub(crate) struct RawWindow {
    bytes: Vec<u8>,
    line_count: usize,
}

impl RawWindow {
    pub(crate) fn from_line_boundary_bytes(bytes: Vec<u8>) -> Self {
        let line_count = memchr::memchr_iter(b'\n', &bytes).count()
            + usize::from(bytes.last().is_some_and(|byte| *byte != b'\n'));
        Self { bytes, line_count }
    }

    pub(crate) fn payload_allocation(&self) -> Option<(usize, usize)> {
        (self.bytes.capacity() > 0).then(|| (self.bytes.as_ptr() as usize, self.bytes.capacity()))
    }

    #[cfg(test)]
    pub(crate) fn audited_payload_allocation(&self) -> Option<(usize, usize)> {
        let Self { bytes, line_count: _ } = self;
        if bytes.capacity() == 0 {
            None
        } else {
            Some((bytes.as_ptr() as usize, bytes.capacity()))
        }
    }

    pub(crate) fn payload_capacity(&self) -> usize {
        self.bytes.capacity()
    }

    pub(crate) fn selection_work_bound(&self, limit: usize) -> usize {
        let kept = limit.min(self.line_count);
        2 * crate::parse::pushed_capacity(kept) * std::mem::size_of::<MatchWindow>()
            + crate::parse::pushed_capacity(kept.saturating_mul(2).min(self.line_count))
                * std::mem::size_of::<Range<usize>>()
    }

    pub(crate) fn select(&self, forms: &[&[u8]], around: usize, limit: usize) -> Vec<Range<usize>> {
        let lines = || self.bytes.split_inclusive(|byte| *byte == b'\n').filter(|line| !line.is_empty());
        let hits = lines().enumerate().filter_map(|(index, line)| {
            forms.iter().any(|form| !form.is_empty() && memchr::memmem::find(line, form).is_some())
                .then_some(index)
        });
        let mut ranges = selected_ranges(self.line_count, hits, around, limit);
        let mut selected = 0;
        let mut offset = 0;
        let mut start = None;
        for (index, line) in lines().enumerate() {
            if selected == ranges.len() {
                break;
            }
            if start.is_none() && index == ranges[selected].start {
                start = Some(offset);
            }
            offset += line.len();
            if index + 1 == ranges[selected].end {
                ranges[selected] = start.take().expect("selected range starts before its end")..offset;
                selected += 1;
            }
        }
        assert_eq!(selected, ranges.len());
        ranges
    }

    pub(crate) fn selected_bytes(&self, range: Range<usize>) -> &[u8] {
        &self.bytes[range]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    fn original_loop(lines: &[bool], around: usize, limit: usize) -> Vec<usize> {
        let mut earliest = Vec::new();
        let mut newest = VecDeque::new();
        let mut before = VecDeque::new();
        let mut active = Rc::new(RefCell::new(Vec::new()));
        let mut after = 0;
        for (position, matched) in lines.iter().enumerate() {
            if *matched {
                active = Rc::new(RefCell::new(before.iter().copied().chain([position]).collect()));
                if earliest.len() < limit {
                    earliest.push(Rc::clone(&active));
                }
                if limit > 0 {
                    if newest.len() == limit {
                        newest.pop_front();
                    }
                    newest.push_back(Rc::clone(&active));
                }
                before.clear();
                after = around;
            } else if after > 0 {
                active.borrow_mut().push(position);
                after -= 1;
            } else if around > 0 {
                if before.len() == around {
                    before.pop_front();
                }
                before.push_back(position);
            }
        }
        let mut windows = Vec::new();
        for window in earliest.into_iter().chain(newest) {
            if !windows.iter().any(|prior| Rc::ptr_eq(prior, &window)) {
                windows.push(window);
            }
        }
        windows.into_iter().flat_map(|window| window.borrow().clone()).collect()
    }

    #[test]
    fn raw_window_ranges_match_original_mutable_window_loop() {
        for count in 0..=12 {
            for bits in 0..1usize << count {
                let lines = (0..count).map(|i| bits & (1 << i) != 0).collect::<Vec<_>>();
                for around in 0..=3 {
                    for limit in 0..=3 {
                        let selected = selected_ranges(count, lines.iter().enumerate().filter_map(|(i, hit)| hit.then_some(i)), around, limit);
                        let actual = selected.into_iter().flatten().collect::<Vec<_>>();
                        assert_eq!(actual, original_loop(&lines, around, limit), "count={count} bits={bits} around={around} limit={limit}");
                    }
                }
            }
        }
    }

    #[test]
    fn raw_window_byte_selection_preserves_legacy_line_windows() {
        for count in 0..=9 {
            for bits in 0..1usize << count {
                let flags = (0..count).map(|i| bits & (1 << i) != 0).collect::<Vec<_>>();
                let lines = flags.iter().enumerate().map(|(i, hit)| {
                    format!("{}-{i}{}", if *hit { "needle" } else { "other" }, if i + 1 == count { "" } else { "\r\n" }).into_bytes()
                }).collect::<Vec<_>>();
                let raw = RawWindow::from_line_boundary_bytes(lines.concat());
                for around in 0..=3 {
                    for limit in 0..=3 {
                        let ranges = raw.select(&[b"needle"], around, limit);
                        assert!(ranges.capacity() * std::mem::size_of::<Range<usize>>() <= raw.selection_work_bound(limit));
                        let actual = ranges.into_iter().flat_map(|range| raw.selected_bytes(range).iter().copied()).collect::<Vec<_>>();
                        let expected = original_loop(&flags, around, limit).into_iter().flat_map(|i| lines[i].iter().copied()).collect::<Vec<_>>();
                        assert_eq!(actual, expected, "count={count} bits={bits} around={around} limit={limit}");
                    }
                }
            }
        }
    }

    #[test]
    fn raw_window_byte_forms_are_literal_and_do_not_retain_line_tables() {
        let source = "ordinary\nquoted \\u00e9\nowner é\nlast".as_bytes().to_vec();
        let capacity = source.capacity();
        let raw = RawWindow::from_line_boundary_bytes(source);
        assert_eq!(raw.payload_capacity(), capacity);
        let selected = raw.select(&[b"\\u00e9", "é".as_bytes()], 0, 20);
        assert_eq!(selected.len(), 2);
        assert_eq!(raw.selected_bytes(selected[0].clone()), b"quoted \\u00e9\n");
        assert_eq!(raw.selected_bytes(selected[1].clone()), "owner é\n".as_bytes());
        assert!(raw.select(&[b""], 2, 20).is_empty());
        assert!(raw.select(&[], 2, 20).is_empty());
        assert!(RawWindow::from_line_boundary_bytes(Vec::new()).select(&[b"x"], 2, 20).is_empty());
    }

    #[test]
    fn raw_window_retains_bounded_ranges_for_dense_hits() {
        for count in [32, 4096, 65536] {
            let selected = selected_ranges(count, 0..count, 2, 20);
            assert_eq!(selected.len(), count.min(40));
            assert_eq!(selected.first(), Some(&(0..1)));
            assert_eq!(selected.last(), Some(&(count - 1..count)));
            assert!(selected.windows(2).all(|ranges| ranges[0].end <= ranges[1].start));
        }
    }
}
