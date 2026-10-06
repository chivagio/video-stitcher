#!/usr/bin/env bash
# Build ONE labeled contact sheet from many screenshots.
#
# Why: headless UI-review sessions capture dozens of PNGs, but a model request
# may include at most 20 images. Reading each screenshot individually blows the
# budget and the request fails with:
#
#   Upstream request failed: [invalid_request_error]
#   a request may include at most 20 images
#
# Tiling the frames into a single image keeps a whole review to one (or a few)
# images, and the per-tile labels preserve which step each frame came from.
#
# Usage:
#   scripts/contact-sheet.sh -o OUT.png [-c COLS] [-w WIDTH] [-n] shot1.png shot2.png ...
#   scripts/contact-sheet.sh -o OUT.png /tmp/opencode/uat-*.png
#
# Options:
#   -o OUT     Output file (required). Format inferred from the extension.
#   -c COLS    Columns in the grid (default: 3).
#   -w WIDTH   Per-tile width in pixels (default: 640). Frames are scaled to
#              this width; height follows the aspect ratio.
#   -n         Do not label tiles with their file name (default: labels on).
#   -h         Show this help.
#
# Requires ImageMagick (`montage`). Exit status is non-zero on any failure.

set -euo pipefail

out=""
cols=3
width=640
label=1

usage() {
    sed -n '2,30p' "$0" | sed 's/^# \{0,1\}//'
}

while getopts ":o:c:w:nh" opt; do
    case "$opt" in
        o) out="$OPTARG" ;;
        c) cols="$OPTARG" ;;
        w) width="$OPTARG" ;;
        n) label=0 ;;
        h) usage; exit 0 ;;
        \?) echo "contact-sheet: unknown option -$OPTARG" >&2; usage >&2; exit 2 ;;
        :) echo "contact-sheet: option -$OPTARG requires a value" >&2; exit 2 ;;
    esac
done
shift $((OPTIND - 1))

if [ -z "$out" ]; then
    echo "contact-sheet: -o OUT.png is required" >&2
    usage >&2
    exit 2
fi
if [ "$#" -eq 0 ]; then
    echo "contact-sheet: no input images given" >&2
    usage >&2
    exit 2
fi

for img in "$@"; do
    if [ ! -f "$img" ]; then
        echo "contact-sheet: input not found: $img" >&2
        exit 1
    fi
done

if ! command -v montage >/dev/null 2>&1; then
    echo "contact-sheet: ImageMagick 'montage' not found on PATH" >&2
    exit 1
fi

args=(-tile "${cols}x" -geometry "${width}x+6+6" -background '#111' -fill '#eee' -pointsize 20)

if [ "$label" -eq 1 ]; then
    # %f is the input file name; shown above each tile so a step can be located.
    args+=(-label '%f')
fi

montage "${args[@]}" "$@" "$out"

# Report the result and a rough image count saved (N tiles -> 1 sheet).
count="$#"
echo "contact-sheet: wrote $out (${count} image(s) -> 1 sheet, ${cols} columns, ${width}px tiles)"
