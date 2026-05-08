#!/usr/bin/env bash
# capture-screenshot.sh — launch jot, wait for the window, capture a clean PNG
# at the window's geometry via grim. Optional --seed swaps in a curated DB
# (backed up + restored on exit).
#
# Usage:
#   scripts/capture-screenshot.sh <name> [--seed <preset>]
#
# Requires: jot, hyprctl, jq, grim, sqlite3.

set -euo pipefail

NAME="${1:-}"
SEED=""
if [[ "${2:-}" == "--seed" ]]; then
  SEED="${3:-}"
fi

if [[ -z "$NAME" ]]; then
  echo "usage: $0 <name> [--seed <preset>]" >&2
  exit 2
fi

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUT_DIR="$REPO_ROOT/docs/screenshots"
OUT_FILE="$OUT_DIR/${NAME}.png"
mkdir -p "$OUT_DIR"

WINDOW_CLASS="com.amageweb.Jot"
DB_DIR="$HOME/.local/share/jot"
DB_FILE="$DB_DIR/notes.db"
DB_BACKUP=""

cleanup() {
  local rc=$?
  if [[ -n "${JOT_PID:-}" ]] && kill -0 "$JOT_PID" 2>/dev/null; then
    kill "$JOT_PID" 2>/dev/null || true
    wait "$JOT_PID" 2>/dev/null || true
  fi
  if [[ -n "$DB_BACKUP" && -f "$DB_BACKUP" ]]; then
    mv -f "$DB_BACKUP" "$DB_FILE"
    echo "restored db from $DB_BACKUP" >&2
  fi
  exit "$rc"
}
trap cleanup EXIT INT TERM

seed_db() {
  local preset="$1"
  mkdir -p "$DB_DIR"
  if [[ -f "$DB_FILE" ]]; then
    DB_BACKUP="$(mktemp -p "$DB_DIR" notes.db.backup.XXXXXX)"
    cp -a "$DB_FILE" "$DB_BACKUP"
  fi
  rm -f "$DB_FILE" "$DB_FILE-wal" "$DB_FILE-shm"

  # Let jot create the schema first (then we'll insert).
  jot >/dev/null 2>&1 &
  local pid=$!
  sleep 0.8
  kill "$pid" 2>/dev/null || true
  wait "$pid" 2>/dev/null || true

  case "$preset" in
    hero)
      sqlite3 "$DB_FILE" <<'SQL'
DELETE FROM notes;
INSERT INTO notes (title, body, pinned, created_at, updated_at) VALUES
  ('Shipping notes',
   'Shipping notes' || char(10) || char(10) ||
   '- Cut a 1.0.0 tag once light theme lands' || char(10) ||
   '- Refresh README screenshots' || char(10) ||
   '- Mention **Sway** support',
   1, datetime('now', '-5 minutes'), datetime('now', '-1 minute')),
  ('Quote — Dijkstra',
   'Quote — Dijkstra' || char(10) || char(10) ||
   '"Simplicity is prerequisite for reliability."',
   0, datetime('now', '-1 hour'), datetime('now', '-10 minutes')),
  ('Coffee experiment',
   'Coffee experiment' || char(10) || char(10) ||
   '`18g in / 36g out / 28s` — bright, slightly sour. Grind 2 notches finer.',
   0, datetime('now', '-2 hours'), datetime('now', '-30 minutes')),
  ('Read later',
   'Read later' || char(10) || char(10) ||
   'https://wayland.app/protocols/' || char(10) ||
   'https://docs.gtk.org/gtk4/',
   0, datetime('now', '-1 day'), datetime('now', '-12 hours'));
SQL
      ;;
    voice)
      sqlite3 "$DB_FILE" <<'SQL'
DELETE FROM notes;
INSERT INTO notes (title, body, pinned, created_at, updated_at) VALUES
  ('Voice memo',
   'Voice memo' || char(10) || char(10) ||
   '(press the mic and start talking — Whisper transcribes into the editor)',
   0, datetime('now', '-1 minute'), datetime('now'));
SQL
      ;;
    *)
      echo "unknown seed preset: $preset" >&2
      exit 3
      ;;
  esac
}

if [[ -n "$SEED" ]]; then
  seed_db "$SEED"
fi

# Kill any running jot, wait for it to disappear from the compositor.
pkill -x jot 2>/dev/null || true
for _ in $(seq 1 20); do
  if ! hyprctl clients -j | jq -e --arg c "$WINDOW_CLASS" 'any(.class == $c)' >/dev/null; then
    break
  fi
  sleep 0.1
done

jot >/dev/null 2>&1 &
JOT_PID=$!

GEOM=""
for _ in $(seq 1 80); do  # ~8 s
  GEOM="$(hyprctl clients -j \
    | jq -r --arg c "$WINDOW_CLASS" '
        [.[] | select(.class == $c)][0]
        | select(. != null)
        | "\(.at[0]) \(.at[1]) \(.size[0]) \(.size[1])"' 2>/dev/null || true)"
  if [[ -n "$GEOM" && "$GEOM" != "null null null null" ]]; then
    break
  fi
  sleep 0.1
done

if [[ -z "$GEOM" ]]; then
  echo "timed out waiting for $WINDOW_CLASS to map" >&2
  exit 1
fi

read -r X Y W H <<< "$GEOM"
echo "captured geometry: ${W}x${H}+${X}+${Y}" >&2

# Settle delay so the first paint + drop shadow are stable.
sleep 0.5

grim -g "${X},${Y} ${W}x${H}" "$OUT_FILE"
echo "wrote $OUT_FILE" >&2
