#!/bin/sh
set -eu
ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
OUT="${1:-$ROOT/target/release}"
cd "$OUT"
sha256sum -c SHA256SUMS
python3 -m json.tool thais-sbom.cdx.json >/dev/null
if [ -f SHA256SUMS.asc ]; then
  gpg --verify SHA256SUMS.asc SHA256SUMS
  if [ -f thais-sbom.cdx.json.asc ]; then
    gpg --verify thais-sbom.cdx.json.asc thais-sbom.cdx.json
  fi
else
  echo "WARNING: SHA256SUMS.asc not present" >&2
  exit 1
fi
echo "release verification passed"
