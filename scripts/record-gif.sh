#!/usr/bin/env bash
# record-gif.sh — record the jot window with wf-recorder, convert to a clean
# GIF via ffmpeg's two-pass palette pipeline.
#
# Usage:
#   scripts/record-gif.sh <name> [seconds] [fps]
#
# Requires: jot already running, hyprctl, jq, wf-recorder, ffmpeg.

set -euo pipefail

NAME="${1:-}"
DURATION="${2:-8}"
FPS="${3:-20}"

if [[ -z "$NAME" ]]; then
  echo "usage: $0 <name> [seconds] [fps]" >&2
  exit 2
fi

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUT_DIR="$REPO_ROOT/docs/screenshots"
mkdir -p "$OUT_DIR"

GIF="$OUT_DIR/${NAME}.gif"
TMPDIR="$(mktemp -d -t jot-record.XXXXXX)"
MP4="$TMPDIR/raw.mp4"
PALETTE="$TMPDIR/palette.png"

WINDOW_CLASS="com.amageweb.Jot"

cleanup() {
  local rc=$?
  if [[ -n "${REC_PID:-}" ]] && kill -0 "$REC_PID" 2>/dev/null; then
    kill -INT "$REC_PID" 2>/dev/null || true
    wait "$REC_PID" 2>/dev/null || true
  fi
  rm -rf "$TMPDIR"
  exit "$rc"
}
trap cleanup EXIT INT TERM

GEOM="$(hyprctl clients -j \
  | jq -r --arg c "$WINDOW_CLASS" '
      [.[] | select(.class == $c)][0]
      | select(. != null)
      | "\(.at[0]),\(.at[1]) \(.size[0])x\(.size[1])"')"

if [[ -z "$GEOM" || "$GEOM" == "null,null nullxnull" ]]; then
  echo "jot window not found — is it running?" >&2
  exit 1
fi

echo "recording $GEOM for ${DURATION}s @ ${FPS}fps..." >&2
echo "(switch to the jot window now — recording starts in 2s)" >&2
sleep 2

wf-recorder -g "$GEOM" -f "$MP4" -r "$FPS" -c libx264 -p crf=18 \
  >/dev/null 2>&1 &
REC_PID=$!

sleep "$DURATION"

kill -INT "$REC_PID"
wait "$REC_PID" 2>/dev/null || true
unset REC_PID

# Two-pass palette pipeline — sharp text, clean gradients.
ffmpeg -y -loglevel error -i "$MP4" \
  -vf "fps=${FPS},scale=iw:ih:flags=lanczos,palettegen=stats_mode=diff" \
  "$PALETTE"

ffmpeg -y -loglevel error -i "$MP4" -i "$PALETTE" \
  -lavfi "fps=${FPS},scale=iw:ih:flags=lanczos[v];[v][1:v]paletteuse=dither=sierra2_4a:diff_mode=rectangle" \
  "$GIF"

SIZE="$(du -h "$GIF" | cut -f1)"
echo "wrote $GIF ($SIZE)" >&2
