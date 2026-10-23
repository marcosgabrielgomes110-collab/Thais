# ADR-0008: Sandbox is enforced by default after M6

- Status: Accepted
- Date: 2026-10-23
- Deciders: Thais maintainers
- Related: ROADMAP M6, README invariants 10 and 12

## Context

The workspace path jail is not an OS sandbox. Bash can read, write, and reach the network outside the workspace. Linux, macOS, and Windows offer different sandbox primitives, and some environments cannot support them.

## Decision

- From M6, Thais enforces the OS sandbox by default where available: Linux Landlock/seccomp, macOS Seatbelt, and Windows Job Objects.
- The user can opt out per workspace or session only with an explicit, warned setting.
- When the sandbox is unavailable or disabled, bash remains `ask`-only and allow-always/prefix allowlists stay disabled.
- Container mode is optional, not the default.
- Sandbox escape tests and residual-risk documentation are required.
- Only with an active sandbox may bash policy allow always or prefix allowlists.

## Consequences

- Secure by default; compatibility problems have a visible escape hatch.
- Cross-platform CI must cover sandbox availability and fallback.
- Users who disable the sandbox keep the stricter bash policy.

## Alternatives considered

- Opt-in sandbox: weaker default.
- Linux-only enforcement: inconsistent across platforms.
- No sandbox: rejected by the security invariants.

## References

- [README.md](../../README.md)
- [ROADMAP.md](../../ROADMAP.md)
