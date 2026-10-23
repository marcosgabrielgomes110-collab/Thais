# ADR-0002: Model policy and fallback

- Status: Accepted
- Date: 2026-10-23
- Deciders: Thais maintainers
- Related: ROADMAP M3, README decisions

## Context

A single model cannot serve planning, building, exploration, and compaction equally well. Cost, context window, latency, and tool-calling support differ. Silent fallback can change answer quality without the user noticing.

## Decision

- Each session has a default model and optional role overrides for `plan`, `build`, `explore`, `general`, and `compact`.
- Each role has an ordered fallback list. Fallback is attempted only on retryable provider failures, is logged as an event, and is visible in the TUI and API.
- `session.set_model` changes the default or a role; a per-turn override is allowed; every change emits `model.changed`.
- The main loop requires tool calling. A model without it is rejected with a clear error.
- Context-window and capability checks run before a turn; an incompatible switch triggers a rebuild or a warning.
- Usage and cost are recorded per turn and per session.

## Consequences

- More configuration surface, but predictable behavior.
- Fallback can degrade quality; the log and status bar make it explicit.
- Provider adapters must expose capabilities and usage.

## Alternatives considered

- One model per session: simpler, but inflexible and hides fallback.
- Automatic routing by benchmark: out of scope for v1.

## References

- [README.md](../../README.md)
- [ROADMAP.md](../../ROADMAP.md)
