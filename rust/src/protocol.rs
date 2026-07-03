use once_cell::sync::Lazy;
use regex::Regex;

// Raw CC-injected protocol strings (filterspec.py DENIAL_PREFIX / USER_SAID_MARKER /
// USER_SAID_TRAILER) and the head-anchored interrupt pattern (filterspec.py
// INTERRUPT_MARKER_GROUPS).
pub(crate) const DENIAL_PREFIX: &str =
    "The user doesn't want to proceed with this tool use. The tool use was rejected";
pub(crate) const USER_SAID_MARKER: &str = "To tell you how to proceed, the user said:\n";
pub(crate) const USER_SAID_TRAILER: &str = "Note: The user's next message";
const INTERRUPT_MARKER_PATTERN: &str = r"^\s*\[Request interrupted by user";

/// The one interrupt-marker regex (filterspec.py INTERRUPT_MARKER_RE): the pattern
/// compiled case-insensitively, anchored at the start of the haystack with leading
/// whitespace tolerated. Shared with mining's structural fallback so both uses
/// carry identical case semantics.
pub(crate) static INTERRUPT_MARKER_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(&format!("(?i){INTERRUPT_MARKER_PATTERN}")).expect("interrupt regex")
});

/// embedded_user_text (filterspec.py embedded_user_text): the verbatim instruction
/// wrapped between the USER_SAID markers, or None.
pub(crate) fn embedded_user_text(content: &str) -> Option<String> {
    let start = content.find(USER_SAID_MARKER)?;
    let after = &content[start + USER_SAID_MARKER.len()..];
    Some(after.split(USER_SAID_TRAILER).next().unwrap_or(after).trim().to_string())
}

/// interrupt_marker (filterspec.py interrupt_marker): the bracketed interrupt prefix
/// at the head of ``text`` (after lstrip; case-insensitive), through the closing
/// ``]`` when present, else the matched marker prefix.
pub(crate) fn interrupt_marker(text: &str) -> Option<&str> {
    let stripped = text.trim_start();
    let matched = INTERRUPT_MARKER_RE.find(stripped)?;
    match stripped.find(']') {
        Some(end) => Some(&stripped[..=end]),
        None => Some(matched.as_str()),
    }
}

/// is_bare_interrupt_marker (filterspec.py is_bare_interrupt_marker): the whole
/// (stripped) text is just the marker.
pub(crate) fn is_bare_interrupt_marker(text: &str) -> bool {
    match interrupt_marker(text) {
        None => false,
        Some(marker) => text.trim()[marker.trim().len()..].trim().is_empty(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_text_extracts_between_markers() {
        let content = format!("{DENIAL_PREFIX}\n\n{USER_SAID_MARKER}do it this way\n{USER_SAID_TRAILER} ...");
        assert_eq!(embedded_user_text(&content), Some("do it this way".to_string()));
    }

    #[test]
    fn embedded_text_missing_marker_is_none() {
        assert_eq!(embedded_user_text("no marker here"), None);
    }

    #[test]
    fn interrupt_marker_through_bracket() {
        assert_eq!(
            interrupt_marker("[Request interrupted by user]"),
            Some("[Request interrupted by user]")
        );
        assert_eq!(
            interrupt_marker("  [Request interrupted by user for tool use]rest"),
            Some("[Request interrupted by user for tool use]")
        );
        assert_eq!(interrupt_marker("hello"), None);
    }

    #[test]
    fn interrupt_marker_is_head_anchored_and_case_folded() {
        assert_eq!(
            interrupt_marker("  [request INTERRUPTED by user for tool use]"),
            Some("[request INTERRUPTED by user for tool use]")
        );
        assert_eq!(interrupt_marker("she typed [Request interrupted by user] mid-text"), None);
    }

    #[test]
    fn bare_marker_detection() {
        assert!(is_bare_interrupt_marker("[Request interrupted by user]"));
        assert!(!is_bare_interrupt_marker("[Request interrupted by user] no do it differently"));
    }
}
