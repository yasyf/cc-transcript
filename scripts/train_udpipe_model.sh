#!/usr/bin/env bash
# Reproducible recipe for cc-transcript's shippable English UDPipe model.
#
# Produces cc_transcript/sentiment/data/en-ewt.udpipe: a UDPipe 1 model
# (tokenizer + tagger + lemmatizer + parser) trained on UD_English-EWT.
#
# Why we train our own instead of using the LINDAT turnkey model:
#   The pretrained english-ewt UD-2.5 UDPipe model is CC BY-NC-SA 4.0
#   (NONCOMMERCIAL) and cannot ship in our PyPI wheel. UD_English-EWT's
#   annotations are CC BY-SA 4.0 (ShareAlike, commercial use OK with
#   attribution), so a model trained only on that treebank — with no external
#   word embeddings — is shippable. See the NOTICE file for attribution.
#
# Pinned inputs (both re-fetched fresh, nothing committed but the .udpipe):
#   udpipe CLI : github.com/ufal/udpipe @ 9158bf7 (reports "1.4.1-dev",
#                UniLib 3.3.1 / MorphoDiTa 1.11.4-dev / Parsito 1.1.1-devel)
#   treebank   : github.com/UniversalDependencies/UD_English-EWT @ tag r2.18
#                (commit b7711cce01cdd4f5fcc0a8199b8a50d951b16c0c)
#
# Requirements: git, make, a C++11 compiler. On macOS use Apple clang
# (/usr/bin/clang++) via CXX — Homebrew LLVM's libc++ <math.h> is incompatible
# with recent macOS SDKs. On Linux the default g++/clang++ works unmodified.
#
# Usage:  scripts/train_udpipe_model.sh
# Runtime: ~2h34m wall-clock on an Apple-silicon laptop — the neural dependency
#          parser dominates and early-stopped at iteration 10; the tokenizer,
#          tagger, and lemmatizer each finish in minutes. Budget hours, not
#          minutes, and expect the parser phase to run unattended.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUT_MODEL="${REPO_ROOT}/cc_transcript/sentiment/data/en-ewt.udpipe"

UDPIPE_REPO="https://github.com/ufal/udpipe.git"
UDPIPE_COMMIT="9158bf7bb9270fd8d22cc06da36ee9e98b64922c"
EWT_REPO="https://github.com/UniversalDependencies/UD_English-EWT.git"
EWT_TAG="r2.18"

# Everything transient lives under a scratch dir; only OUT_MODEL is kept.
WORK="$(mktemp -d "${TMPDIR:-/tmp}/udpipe-train.XXXXXX")"
trap 'rm -rf "$WORK"' EXIT

# Apple clang on macOS; leave the toolchain default elsewhere.
if [[ "$(uname -s)" == "Darwin" ]]; then
  CXX_OVERRIDE=(CXX=/usr/bin/clang++)
else
  CXX_OVERRIDE=()
fi

echo ">> Building udpipe CLI from source (${UDPIPE_COMMIT:0:7})"
git clone --filter=blob:none "$UDPIPE_REPO" "$WORK/udpipe"
git -C "$WORK/udpipe" checkout --quiet "$UDPIPE_COMMIT"
make -C "$WORK/udpipe/src" "${CXX_OVERRIDE[@]}" -j"$(getconf _NPROCESSORS_ONLN)"
UDPIPE_BIN="$WORK/udpipe/src/udpipe"
"$UDPIPE_BIN" --version

echo ">> Fetching UD_English-EWT @ ${EWT_TAG}"
git clone --depth 1 --branch "$EWT_TAG" "$EWT_REPO" "$WORK/ewt"

echo ">> Training tokenizer + tagger + lemmatizer + parser (all defaults)"
# No external word embeddings: keeps the model a pure derivative of the
# CC BY-SA treebank and fully self-contained / reproducible. The parser phase
# is the long pole (~2h+); it early-stops around iteration 10.
time "$UDPIPE_BIN" --train "$OUT_MODEL" \
  --heldout "$WORK/ewt/en_ewt-ud-dev.conllu" \
  "$WORK/ewt/en_ewt-ud-train.conllu"

echo ">> Accuracy on held-out test set (en_ewt-ud-test.conllu)"
"$UDPIPE_BIN" --accuracy \
  --tokenizer= --tagger= --parser= \
  "$OUT_MODEL" "$WORK/ewt/en_ewt-ud-test.conllu"

printf '>> Wrote %s (%s bytes)\n' "$OUT_MODEL" "$(wc -c < "$OUT_MODEL")"
