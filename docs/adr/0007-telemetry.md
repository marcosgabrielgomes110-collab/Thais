# ADR-0007: No telemetry in v1

- Status: Accepted
- Date: 2026-10-23
- Deciders: Thais maintainers
- Related: ROADMAP Observability, README principles

## Context

Telemetry can improve prioritization, but Thais handles source code, prompts, paths, and possibly secrets. Local-first and secure-by-default make unsolicited network calls unacceptable in v1.

## Decision

- v1 sends no telemetry, analytics, or usage data.
- Logs, metrics, and traces stay local by default.
- Crash reports are local unless the user explicitly exports them.
- Any future telemetry is opt-in, off by default, aggregate, documented, and free of code, prompts, paths, and secrets.
- Adding telemetry requires a new ADR and a release note.

## Consequences

- Less product data; decisions rely on Alpha feedback and local diagnostics.
- Privacy and security posture stay strong.
- `thais doctor` and local logs carry the debugging burden.

## Alternatives considered

- Opt-in anonymous metrics: reasonable later, not v1.
- Opt-in detailed metrics: too much privacy risk for v1.

## References

- [README.md](../../README.md)
- [ROADMAP.md](../../ROADMAP.md)
