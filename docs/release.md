# Release

## Local build

```sh
./scripts/release.sh v1.0.0 --allow-unsigned
./scripts/verify-release.sh
```

## cargo-dist artifacts

```sh
cargo install cargo-dist --locked --version 0.30.0
./scripts/dist-local.sh v0.1.0
```

This builds the `dist` profile, x86_64 Linux archives, checksums, and the
`thais-cli-installer.sh` / `thais-sandbox-installer.sh` installers under
`target/distrib/`. The script verifies every `.sha256` file. `cargo-dist`
skips the PowerShell installer for a Linux-only target set; Windows targets
are post-v1.

`--allow-unsigned` is for local checks only. A stable release requires a GPG key.

## Signing key

Export the private key into the CI secret `THAIS_SIGNING_KEY`:

```sh
gpg --armor --export-secret-keys <fingerprint> > private-key.asc
gh secret set THAIS_SIGNING_KEY < private-key.asc
```

`scripts/release.sh` imports the armored key automatically when the secret
contains a PGP private key. A fingerprint is also accepted when the key is
already present in the local keyring.

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
