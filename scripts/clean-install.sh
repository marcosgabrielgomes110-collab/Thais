#!/bin/sh
set -eu
# Clean-install smoke test. Uses Docker when available; otherwise a clean
# prefix from source. The measured window includes build/install and the first
# successful fake-provider task.
ROOT=$(mktemp -d)
trap 'rm -rf "$ROOT"' EXIT
START=$(date +%s)

if command -v docker >/dev/null 2>&1; then
  docker build -t thais-clean-install .
  OUTPUT=$(docker run --rm thais-clean-install run --task hello --json)
else
  echo "docker not found; using a clean local prefix fallback" >&2
  export CARGO_HOME="$ROOT/cargo"
  export CARGO_TARGET_DIR="$ROOT/target"
  cargo build --release --locked --workspace >/dev/null
  mkdir -p "$ROOT/bin"
  cp "$CARGO_TARGET_DIR/release/thais" "$ROOT/bin/thais"
  cp "$CARGO_TARGET_DIR/release/thais-sandbox" "$ROOT/bin/thais-sandbox"
  OUTPUT=$("$ROOT/bin/thais" run --task hello --json)
fi

END=$(date +%s)
echo "$OUTPUT" | tail -1
echo "clean_install_seconds=$((END - START))"
echo "$OUTPUT" | grep -q 'M0 fake provider ready' || { echo "first task failed" >&2; exit 1; }
