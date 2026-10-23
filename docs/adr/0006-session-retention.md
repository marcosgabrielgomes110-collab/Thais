# ADR-0006: Session retention

- Status: Accepted
- Date: 2026-10-23
- Deciders: Thais maintainers
- Related: ROADMAP M3, storage layout

## Context

Sessions are user data. Local-first software should not delete work on a timer. Indefinite retention grows disk usage, but storage is cheap and deletion can be explicit.

## Decision

- Sessions are archived indefinitely in v1.
- There is no automatic expiry or background deletion.
- Deletion is an explicit user action through the API or CLI, with confirmation.
- Archived sessions remain searchable and exportable.
- `thais doctor` reports data size so the user can see growth.
- Any future retention policy is opt-in configuration and requires a new ADR.

## Consequences

- No surprise data loss.
- Disk usage grows until the user acts.
- Backup and export guidance belongs in the docs.

## Alternatives considered

- TTL expiry: risks deleting active work.
- Keep the N most recent: arbitrary and surprising.

## References

- [README.md](../../README.md)
- [ROADMAP.md](../../ROADMAP.md)
