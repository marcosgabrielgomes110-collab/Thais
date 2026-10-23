# ADR-0009: Credential storage and fallback

- Status: Accepted
- Date: 2026-10-23
- Deciders: Thais maintainers
- Related: ROADMAP Storage and credentials, README invariant 11

## Context

API keys must never enter config, events, prompts, or snapshots. Keyrings are not available in every headless or CI environment. A fallback is needed, but it must be explicit and visible.

## Decision

- Precedence: environment (`THAIS_<PROVIDER>_API_KEY`), then OS keyring, then a `0600` `credentials.toml` fallback only when the keyring is unavailable.
- The fallback warns on use. It is never silent.
- Keyring service is `thais`; account is `<provider>:<account>`.
- `thais auth login`, `logout`, and `status` manage credentials. Full keys are never printed; show a fingerprint or the last four characters.
- Provider config stores auth references, not secrets.
- Writes are atomic and do not follow symlinks.
- Tests use a fake keyring and a temporary XDG root.

## Consequences

- CI and headless use work.
- The fallback is less secure but explicit and detectable by `thais doctor`.
- Users can choose environment-only by not creating the fallback file.

## Alternatives considered

- Environment-only: poor interactive UX.
- Encrypted file: extra key-management burden.
- Keyring-only: breaks CI and headless Linux.

## References

- [README.md](../../README.md)
- [ROADMAP.md](../../ROADMAP.md)
