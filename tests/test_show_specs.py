"""The public ``NOISE_SPEC`` builder stays byte-identical to the core filter spec.

``rust/crates/core/src/filter.rs`` owns the one ``NOISE_SPEC_JSON`` the ``show``
command and the parse bench serve; ``builders.NOISE_SPEC`` is the Python builder that
produces the same spec. This pins them together so a change to the structural junk
groups on one side can't silently drift from the other.
"""

from __future__ import annotations

from tests.support import requires_rust


@requires_rust
def test_noise_spec_matches_native() -> None:
    from cc_transcript import _native
    from cc_transcript.builders import NOISE_SPEC
    from cc_transcript.filterspec import spec_to_json

    assert spec_to_json(NOISE_SPEC) == _native.noise_spec_json()
