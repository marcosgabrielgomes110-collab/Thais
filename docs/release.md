# Release

## Local build

```sh
./scripts/release.sh v1.0.0 --allow-unsigned
./scripts/verify-release.sh
```

`--allow-unsigned` is for local checks only. A stable release requires a GPG key.

## Signing key

Export the private key into the CI secret `THAIS_SIGNING_KEY`:

```sh
gpg --armor --export-secret-keys <fingerprint>
gh secret set THAIS_SIGNING_KEY < private-key.asc
```

The public key must be published with the release so users can verify:

```sh
gpg --verify SHA256SUMS.asc SHA256SUMS
sha256sum -c SHA256SUMS
```

## Artifacts

- `thais` and `thais-sandbox`.
- `thais-sbom.cdx.json` (CycloneDX 1.5, generated from `Cargo.lock`).
- `SHA256SUMS`, `SHA256SUMS.asc`.
- SBOM signature.

The tag workflow `.github/workflows/release.yml` runs `make check`, builds,
generates the SBOM, signs, uploads, and creates the GitHub release. It fails if
the signing secret is missing.
