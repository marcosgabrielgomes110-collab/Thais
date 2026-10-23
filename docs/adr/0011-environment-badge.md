# ADR-0011: Environment badge privacy and refresh

- Status: Accepted
- Date: 2026-10-23
- Deciders: Thais maintainers
- Related: ROADMAP Environment badge, README prompt assembly

## Context

A small environment summary saves tool calls, but environment data can leak identity, paths, and secrets. It also goes stale.

## Decision

- Include OS, kernel, architecture, allowlisted shell, relativized workspace, git repository/branch/dirty state, coarse CPU/RAM, allowlisted tools, project markers, and date.
- Omit the username and home directory by default; relativize paths.
- Never dump the full environment, secrets, tokens, proxy credentials, SSH agent sockets, or cloud metadata.
- Cache the badge and refresh it on session start, workspace change, and `thais env refresh`.
- Do not file-watch in v1.
- Keep a small token budget; truncate the tool list before OS and workspace facts.
- Treat the badge as data, not instructions.

## Consequences

- Fewer discovery calls and a stable prompt section.
- Facts can be stale between refreshes.
- Leakage tests cover the badge and prompt assembly.

## Alternatives considered

- Include username and home: unnecessary privacy risk.
- File-watch refresh: complexity for v1.
- Manual-only refresh: too easy to drift.

## References

- [README.md](../../README.md)
- [ROADMAP.md](../../ROADMAP.md)
