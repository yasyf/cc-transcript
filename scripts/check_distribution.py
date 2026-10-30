from __future__ import annotations

import json
import sys
import tarfile
import zipfile
from hashlib import sha256
from pathlib import Path

import cc_transcript
from cc_transcript.nlp import analyze
from cc_transcript.sentiment.lexicon import AFINN, DOMAIN_OVERRIDES, Lexicon, tokenize

MODEL_PATH = "cc_transcript/sentiment/data/en-ewt.udpipe"


def main() -> None:
    directory = Path(sys.argv[1])
    (wheel,) = directory.glob("*.whl")
    (sdist,) = directory.glob("*.tar.gz")
    with zipfile.ZipFile(wheel) as archive:
        names = archive.namelist()
        assert MODEL_PATH not in names
        assert "cc_transcript/sentiment/data/afinn-en-165.tsv" in names
        assert "cc_transcript/sentiment/data/domain_overrides.tsv" in names
        (native,) = (
            entry
            for entry in archive.infolist()
            if entry.filename.startswith("cc_transcript/_native.") and entry.filename.endswith(".so")
        )
    with tarfile.open(sdist) as archive:
        (model,) = (entry for entry in archive if entry.name.endswith(f"/{MODEL_PATH}"))
        archived_model = archive.extractfile(model)
        assert archived_model is not None
        assert (
            sha256(archived_model.read()).digest()
            == sha256(Path(__file__).resolve().parents[1].joinpath(MODEL_PATH).read_bytes()).digest()
        )
    assert Path(cc_transcript.__file__).is_relative_to(Path(sys.prefix))
    assert [token.form for token in analyze("can't believe this isn't broken")] == [
        "ca",
        "n't",
        "believe",
        "this",
        "is",
        "n't",
        "broken",
    ]
    assert tokenize("can't") == ["ca", "n't"]
    assert Lexicon.has_hit("this isn't amazing", want_negative=True)
    assert AFINN and DOMAIN_OVERRIDES["broken"] == -3
    print(
        json.dumps(
            {
                "wheel": wheel.name,
                "wheel_bytes": wheel.stat().st_size,
                "wheel_sha256": sha256(wheel.read_bytes()).hexdigest(),
                "wheel_model_count": names.count(MODEL_PATH),
                "wheel_members": names,
                "wheel_native_bytes": native.file_size,
                "wheel_native_compressed_bytes": native.compress_size,
                "sdist": sdist.name,
                "sdist_bytes": sdist.stat().st_size,
                "sdist_model_bytes": model.size,
                "installed_package": cc_transcript.__file__,
                "installed_prefix": sys.prefix,
                "nlp_api": "PASS",
            }
        )
    )


if __name__ == "__main__":
    main()
