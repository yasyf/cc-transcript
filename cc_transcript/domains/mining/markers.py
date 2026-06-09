"""Claude Code transcript marker constants the mining fact-detectors recognize."""

from __future__ import annotations

from cc_transcript import INTERRUPT_MARKER_RE as INTERRUPT_MARKER_RE

DENIAL_PREFIX = "The user doesn't want to proceed with this tool use. The tool use was rejected"
USER_SAID_MARKER = "To tell you how to proceed, the user said:\n"
USER_SAID_TRAILER = "Note: The user's next message"
EDIT_TOOLS = frozenset({"Edit", "Write", "MultiEdit", "NotebookEdit"})
REENTRY_LOOKBACK = 40
