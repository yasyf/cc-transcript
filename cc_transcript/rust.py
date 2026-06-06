from __future__ import annotations

from pathlib import Path
from typing import TYPE_CHECKING, ClassVar, Literal

import anyio
import anyio.to_thread

from cc_transcript import _parser_rs as rust
from cc_transcript.backend import ParsedTranscript

if TYPE_CHECKING:
    from collections.abc import AsyncIterator, Sequence


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
    ) -> AsyncIterator[ParsedTranscript]:
        """See :meth:`Backend.parse_batch`."""
        if not paths:
            return
        stream = rust.stream_parse([(str(path), mtime) for path, mtime in paths], prefetch)
        while batch := await anyio.to_thread.run_sync(stream.recv_many, self.recv_batch):
            for path, mtime, events in batch:
                yield ParsedTranscript(path=Path(path), mtime=mtime, events=tuple(events))


__all__ = ["RustBackend"]
