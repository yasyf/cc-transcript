//! Python ``str`` whitespace semantics: ``isspace``/``strip``/``split`` treat the C0
//! separators U+001C–U+001F as whitespace, which ``char::is_whitespace`` omits.

/// Whether ``c`` is whitespace under Python ``str.isspace``: Unicode White_Space plus
/// the C0 separators U+001C–U+001F that ``char::is_whitespace`` omits.
pub fn is_space(c: char) -> bool {
    c.is_whitespace() || matches!(c, '\u{1c}'..='\u{1f}')
}

/// Python ``str.strip()``: the input without leading and trailing Python whitespace.
pub fn strip(s: &str) -> &str {
    s.trim_matches(is_space)
}

/// Python ``str.split()`` with no separator: maximal non-whitespace runs, no empties.
pub fn split_whitespace(s: &str) -> impl Iterator<Item = &str> {
    s.split(is_space).filter(|piece| !piece.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn c0_separators_are_whitespace() {
        assert!(
            is_space('\u{1c}') && is_space('\u{1d}') && is_space('\u{1e}') && is_space('\u{1f}')
        );
        assert!(is_space(' ') && is_space('\t') && is_space('\u{a0}') && is_space('\u{0b}'));
        assert!(!is_space('a') && !is_space('\u{200b}'));
    }

    #[test]
    fn strip_removes_c0_separators() {
        assert_eq!(strip("\u{1c}\u{1d}\u{1e}\u{1f}"), "");
        assert_eq!(strip("  hi \u{1f}"), "hi");
        assert_eq!(strip("keep"), "keep");
    }

    #[test]
    fn split_breaks_on_c0_separators() {
        assert_eq!(split_whitespace("a\u{1f}b").collect::<Vec<_>>(), ["a", "b"]);
        assert_eq!(split_whitespace("  a  b  ").collect::<Vec<_>>(), ["a", "b"]);
        assert!(split_whitespace(" \u{1c} ").next().is_none());
    }
}
