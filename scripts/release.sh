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
  signing_key="$THAIS_SIGNING_KEY"
  if printf '%s' "$signing_key" | grep -q -- 'BEGIN PGP PRIVATE KEY'; then
    printf '%s' "$signing_key" | gpg --batch --import >/dev/null 2>&1
    signing_key=$(gpg --list-secret-keys --with-colons | awk -F: '$1 == "fpr" { print $10; exit }')
  fi
  if [ -z "$signing_key" ]; then
    echo "could not resolve a GPG signing key" >&2
    exit 1
  fi
  gpg --batch --yes --local-user "$signing_key" --armor --detach-sign \
    --output "$OUT/SHA256SUMS.asc" "$OUT/SHA256SUMS"
  gpg --batch --yes --local-user "$signing_key" --armor --detach-sign \
    --output "$OUT/thais-sbom.cdx.json.asc" "$OUT/thais-sbom.cdx.json"
  echo "signed with $signing_key"
elif [ "${2:-}" = "--allow-unsigned" ]; then
  echo "WARNING: unsigned artifacts; not a stable release" >&2
else
  echo "THAIS_SIGNING_KEY is required; pass --allow-unsigned only for local checks" >&2
  exit 1
fi
echo "artifacts ready for $VERSION in $OUT"
