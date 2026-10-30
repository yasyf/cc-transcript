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
