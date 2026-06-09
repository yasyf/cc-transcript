"""Back-compat shim: the pre-0.6 ``cc_transcript.sentiment.*`` import paths still
resolve to the relocated domain (``cc_transcript.domains.sentiment``) and the
promoted core message types. Delete with the shim once consumers have migrated."""

from __future__ import annotations


def test_sentiment_package_shim_reexports_domain() -> None:
    from cc_transcript.domains.sentiment import FilteredEngine, ScoreSpec
    from cc_transcript.sentiment import FilteredEngine as ShimFilteredEngine
    from cc_transcript.sentiment import ScoreSpec as ShimScoreSpec

    assert ShimFilteredEngine is FilteredEngine
    assert ShimScoreSpec is ScoreSpec


def test_sentiment_submodule_shims_resolve() -> None:
    from cc_transcript.domains.sentiment.buckets import ConversationBucketer
    from cc_transcript.domains.sentiment.lexicon import Lexicon
    from cc_transcript.sentiment.buckets import ConversationBucketer as ShimBucketer
    from cc_transcript.sentiment.lexicon import Lexicon as ShimLexicon

    assert ShimBucketer is ConversationBucketer
    assert ShimLexicon is Lexicon


def test_message_types_shim_to_core() -> None:
    from cc_transcript import UserMessage
    from cc_transcript.sentiment import UserMessage as PkgShim
    from cc_transcript.sentiment.messages import UserMessage as SubShim

    assert PkgShim is SubShim is UserMessage
