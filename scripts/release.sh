#!/bin/sh
set -eu
VERSION="${1:?usage: scripts/release.sh <version> [--allow-unsigned]}"
ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
OUT="$ROOT/target/release"

cd "$ROOT"
cargo build --release --locked --workspace
python3 "$ROOT/scripts/sbom.py" "$OUT/thais-sbom.cdx.json"
(cd "$OUT" && sha256sum thais thais-sandbox thais-sbom.cdx.json > SHA256SUMS)

if [ -n "${THAIS_SIGNING_KEY:-}" ]; then
  gpg --batch --yes --local-user "$THAIS_SIGNING_KEY" --armor --detach-sign \
    --output "$OUT/SHA256SUMS.asc" "$OUT/SHA256SUMS"
  gpg --batch --yes --local-user "$THAIS_SIGNING_KEY" --armor --detach-sign \
    --output "$OUT/thais-sbom.cdx.json.asc" "$OUT/thais-sbom.cdx.json"
  echo "signed with $THAIS_SIGNING_KEY"
elif [ "${2:-}" = "--allow-unsigned" ]; then
  echo "WARNING: unsigned artifacts; not a stable release" >&2
else
  echo "THAIS_SIGNING_KEY is required; pass --allow-unsigned only for local checks" >&2
  exit 1
fi
echo "artifacts ready for $VERSION in $OUT"
