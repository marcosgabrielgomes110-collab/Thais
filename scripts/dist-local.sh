#!/bin/sh
set -eu
ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
DIST=${DIST:-dist}
TAG=${1:-v0.1.0}
TARGET=x86_64-unknown-linux-gnu
cd "$ROOT"
if ! command -v "$DIST" >/dev/null 2>&1; then
  echo "cargo-dist is required: cargo install cargo-dist --locked --version 0.30.0" >&2
  exit 1
fi
echo "== dist plan =="
"$DIST" plan
START=$(date +%s)
echo "== dist build =="
"$DIST" build --artifacts=all --target "$TARGET" --tag "$TAG" --allow-dirty
SECONDS_RUN=$(( $(date +%s) - START ))
echo "dist_build_seconds=$SECONDS_RUN"
echo "== checksums =="
(cd target/distrib && for checksum in *.sha256; do sha256sum -c "$checksum"; done)
echo "== artifacts =="
find target/distrib -maxdepth 1 -type f -printf '%f %s bytes\n' | sort
