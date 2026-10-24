#!/bin/sh
set -eu
ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$ROOT"
cargo build --release --locked --workspace >/dev/null
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT
mkdir -p "$TMP/home" "$TMP/config" "$TMP/data" "$TMP/state" "$TMP/cache" "$TMP/runtime"
export HOME="$TMP/home"
export XDG_CONFIG_HOME="$TMP/config"
export XDG_DATA_HOME="$TMP/data"
export XDG_STATE_HOME="$TMP/state"
export XDG_CACHE_HOME="$TMP/cache"
export XDG_RUNTIME_DIR="$TMP/runtime"
export THAIS_SANDBOX=off
BIN="$ROOT/target/release/thais"
now_ms() { date +%s%N | awk '{printf "%d", $1 / 1000000}'; }
total=0
for _ in 1 2 3 4 5; do
  start=$(now_ms)
  "$BIN" run --task hello --json >/dev/null
  elapsed=$(( $(now_ms) - start ))
  total=$((total + elapsed))
done
average=$((total / 5))
start=$(now_ms)
"$BIN" events >/dev/null
events_ms=$(( $(now_ms) - start ))
printf 'first_task_avg_ms=%s\n' "$average"
printf 'events_ms=%s\n' "$events_ms"
if [ "$average" -gt 5000 ]; then
  echo "first task exceeded 5000 ms" >&2
  exit 1
fi
if [ "$events_ms" -gt 2000 ]; then
  echo "event log read exceeded 2000 ms" >&2
  exit 1
fi
