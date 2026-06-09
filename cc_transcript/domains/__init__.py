"""Built-in domains layered on the cc-transcript core.

Each domain builds on the core transcript model and depends only on core — never on
another domain, and never the reverse. Heavy dependencies sit behind a per-domain
extra. Today: :mod:`cc_transcript.domains.sentiment` (scoring) and
:mod:`cc_transcript.domains.mining` (correction/feedback extraction).
"""

from __future__ import annotations
