#!/bin/sh
# Regenerate docs/assets/demo.png from a real `uvx cc-transcript stats` run.
# Requires freeze (https://github.com/charmbracelet/freeze) and bat on PATH.
# The pipe through `fold` reproduces what a 96-column terminal shows; bat adds
# the syntax colors that freeze then renders via --language ansi.
set -eu
cd "$(dirname "$0")/../.."
tmp=$(mktemp)
frame="$tmp.ansi"
trap 'rm -f "$tmp" "$frame"' EXIT
{
    printf '$ uvx cc-transcript stats --project cc-transcript --limit 1\n' |
        bat --plain --color=always --language bash
    uvx cc-transcript stats --project cc-transcript --limit 1 | fold -s -w 96 |
        bat --plain --color=always --language help
} >"$frame"
freeze "$frame" --language ansi --theme github-dark --background "#0d1117" --window --padding 24 \
    --font.size 28 --font.family Menlo --output docs/assets/demo.png
