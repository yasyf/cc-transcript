#!/bin/sh
# Regenerate docs/assets/demo.png from a real `uvx cc-transcript stats` run.
# Requires freeze (https://github.com/charmbracelet/freeze) on PATH.
# The pipe through `fold` reproduces what a 96-column terminal shows.
set -eu
cd "$(dirname "$0")/../.."
frame=$(mktemp)
trap 'rm -f "$frame"' EXIT
{
    printf '$ uvx cc-transcript stats --project cc-transcript --limit 1\n'
    uvx cc-transcript stats --project cc-transcript --limit 1 | fold -s -w 96
} >"$frame"
freeze "$frame" --theme github-dark --background "#0d1117" --window --padding 24 \
    --font.size 28 --font.family Menlo --output docs/assets/demo.png
