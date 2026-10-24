#!/bin/sh
set -eu
ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
SECONDS_RUN="${1:-30}"
cd "$ROOT/fuzz"
if ! command -v cargo-fuzz >/dev/null 2>&1; then
  echo "cargo-fuzz is required: cargo install cargo-fuzz --locked" >&2
  exit 1
fi
cargo fuzz run protocol -- -max_total_time="$SECONDS_RUN" -rss_limit_mb=2048
cargo fuzz run tool_args -- -max_total_time="$SECONDS_RUN" -rss_limit_mb=2048
