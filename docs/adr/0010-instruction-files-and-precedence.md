# ADR-0010: Instruction files and precedence

- Status: Accepted
- Date: 2026-10-23
- Deciders: Thais maintainers
- Related: ROADMAP Instruction files, README invariant 13

## Context

Human-authored instructions and agent-learned memory serve different purposes. Repository content is untrusted and could try to override security policy. The model needs a stable, labeled order.

## Decision

- Load the global `~/.config/thais/AGENTS.md` and the project-root `<repo>/AGENTS.md`.
- Do not load a hidden `.thais/AGENTS.md` in v1.
- Nested per-directory `AGENTS.md` files are a later extension.
- Precedence: safety invariants, then project `AGENTS.md`, then global `AGENTS.md`, then `MEMORY.md`, then session checkpoint and task state.
- Merge sections with explicit labels; enforce size and token caps with warnings, not silent truncation.
- Project `AGENTS.md` is untrusted for security and cannot override the gate, sandbox, approvals, or secret policy.
- Reload on file change or an explicit command; show which files were loaded.

## Consequences

- A clear hierarchy and a smaller v1 file-discovery surface.
- Malicious-instruction tests are required.
- Prompt assembly stays stable across rebuilds.

## Alternatives considered

- Also load `.thais/AGENTS.md`: more config surface without a clear v1 need.
- `MEMORY.md` above `AGENTS.md`: learned text could override human intent; rejected.

## References

- [README.md](../../README.md)
- [ROADMAP.md](../../ROADMAP.md)
