#!/bin/sh
set -eu
VERSION="${1:?usage: scripts/release.sh <version>}"
cargo build --release --locked --workspace
cargo cyclonedx --format json --output target/thais-sbom.json
( cd target/release && sha256sum thais thais-sandbox > SHA256SUMS )
echo "Artifacts ready for $VERSION in target/release"
echo "Sign with: gpg --armor --detach-sign target/release/SHA256SUMS"
