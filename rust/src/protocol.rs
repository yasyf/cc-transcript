use once_cell::sync::Lazy;
use regex::Regex;

use crate::generated::protocol::{AGENT_INJECTION_PATTERN, INTERRUPT_MARKER_PATTERN};

pub use crate::generated::protocol::{
    ANSWERED_PREFIX, ANSWERED_TRAILER, DENIAL_PREFIX, USER_SAID_MARKER, USER_SAID_TRAILER,
};

/// The one interrupt-marker regex (filterspec.py INTERRUPT_MARKER_RE): the pattern
/// compiled case-insensitively, anchored at the start of the haystack with leading
/// whitespace tolerated. Shared with mining's structural fallback so both uses
/// carry identical case semantics.
pub static INTERRUPT_MARKER_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(&format!("(?i){INTERRUPT_MARKER_PATTERN}")).expect("interrupt regex")
});

/// The agent-injection regex (filterspec.py AGENT_INJECTION_RE): the group
/// alternation compiled case-insensitively, matched with search semantics
/// (filterspec.py is_agent_injection uses .search()).
pub static AGENT_INJECTION_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(&format!("(?i){AGENT_INJECTION_PATTERN}")).expect("agent injection regex")
});

/// is_agent_injection (filterspec.py is_agent_injection): whether ``text`` is an
/// agent-injected relay banner rather than an authored prompt.
pub fn is_agent_injection(text: &str) -> bool {
    AGENT_INJECTION_RE.is_match(text)
}

/// embedded_user_text (filterspec.py embedded_user_text): the verbatim instruction
/// wrapped between the USER_SAID markers, or None.
pub fn embedded_user_text(content: &str) -> Option<String> {
    let start = content.find(USER_SAID_MARKER)?;
    let after = &content[start + USER_SAID_MARKER.len()..];
    Some(after.split(USER_SAID_TRAILER).next().unwrap_or(after).trim().to_string())
}

/// interrupt_marker (filterspec.py interrupt_marker): the bracketed interrupt prefix
/// at the head of ``text`` (after lstrip; case-insensitive), through the closing
/// ``]`` when present, else the matched marker prefix.
pub fn interrupt_marker(text: &str) -> Option<&str> {
    let stripped = text.trim_start();
    let matched = INTERRUPT_MARKER_RE.find(stripped)?;
    match stripped.find(']') {
        Some(end) => Some(&stripped[..=end]),
        None => Some(matched.as_str()),
    }
}

/// is_bare_interrupt_marker (filterspec.py is_bare_interrupt_marker): the whole
/// (stripped) text is just the marker.
pub fn is_bare_interrupt_marker(text: &str) -> bool {
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

    #[test]
    fn agent_injection_matches_relay_banners() {
        assert!(is_agent_injection("<teammate-message from='r'>rebase</teammate-message>"));
        assert!(is_agent_injection("<scheduled-task id='7'>run the suite</scheduled-task>"));
        assert!(is_agent_injection("[Role Reminder: You are a Coordinator."));
        assert!(is_agent_injection("# Augment Agent\nyou have these tools"));
        // Leading whitespace before the marker is tolerated.
        assert!(is_agent_injection("   <teammate-message from='mate'>ping</teammate-message>"));
    }

    #[test]
    fn agent_injection_rejects_prose_and_mid_text_mentions() {
        assert!(!is_agent_injection("remind me what the teammate coordinator does"));
        // Start-anchored: a relay tag mentioned mid-text is authored, not injected.
        assert!(!is_agent_injection("as noted in the <teammate-message> above"));
        assert!(!is_agent_injection("Why did the transcript contain <teammate-message from=a>?"));
        assert!(!is_agent_injection("we discussed the [Role Reminder] banner mid-sentence"));
        // A combining mark (U+0301) after the tag name is not a portable word boundary; and
        // dotless-I must not fold to ASCII "i" — Python re once matched both, Rust never did.
        assert!(!is_agent_injection("<teammate-message\u{0301}>"));
        assert!(!is_agent_injection("[Role Rem\u{0131}nder: dotless"));
        assert!(!is_agent_injection(""));
    }
}
