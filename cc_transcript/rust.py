from __future__ import annotations

from pathlib import Path
from typing import TYPE_CHECKING, ClassVar, Literal

import anyio
import anyio.to_thread

from cc_transcript import _parser_rs as rust
from cc_transcript.backend import ParsedTranscript
from cc_transcript.filterspec import apply_spec, is_portable, spec_to_json

if TYPE_CHECKING:
    from collections.abc import AsyncIterator, Sequence

    from cc_transcript.filterspec import FilterSpec


class RustBackend:
    """The Rust parsing backend, at full event parity with :class:`PythonBackend`.

    Streams files through a rayon thread pool inside the extension module and
    materializes :class:`~cc_transcript.models.TranscriptEvent` objects on the
    consumer side, draining the stream off the event loop via :mod:`anyio`
    worker threads.
    """

    name: ClassVar[Literal["rust", "python"]] = "rust"
    recv_batch: ClassVar[int] = 32

    async def parse_batch(
        self,
        paths: Sequence[tuple[Path, float]],
        *,
        prefetch: int,
        spec: FilterSpec | None = None,
    ) -> AsyncIterator[ParsedTranscript]:
        """See :meth:`Backend.parse_batch`.

        A portable ``spec`` is executed inside Rust (events dropped before
        materialization); a non-portable one is applied in Python post-parse.
        """
        if not paths:
            return
        rust_spec = spec_to_json(spec) if spec is not None and is_portable(spec) else None
        python_spec = spec if spec is not None and rust_spec is None else None
        stream = rust.stream_parse([(str(path), mtime) for path, mtime in paths], prefetch, rust_spec)
        while batch := await anyio.to_thread.run_sync(stream.recv_many, self.recv_batch):
            for path, mtime, events in batch:
                kept = tuple(apply_spec(events, python_spec)) if python_spec is not None else tuple(events)
                yield ParsedTranscript(path=Path(path), mtime=mtime, events=kept)
