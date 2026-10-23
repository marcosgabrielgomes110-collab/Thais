# ADR-0004: LiteLLM is optional

- Status: Accepted
- Date: 2026-10-23
- Deciders: Thais maintainers
- Related: ROADMAP M3, ADR-0003

## Context

LiteLLM can normalize many providers behind one schema, but it adds a proxy dependency and hides provider-specific tools, thinking, usage, and errors.

## Decision

- LiteLLM is supported as an optional `openai-compatible` endpoint.
- It is never required, never the default normalization layer, and never the only way to reach a provider.
- Native OpenAI and Anthropic adapters remain first-class.
- The session layer does not special-case LiteLLM.

## Consequences

- Users who want one schema can opt in.
- Thais keeps control of native features and errors.
- Documentation must state the limitations and that LiteLLM is not a release blocker.

## Alternatives considered

- LiteLLM as the default layer: rejected because of the extra dependency and loss of provider control.
- No LiteLLM support: rejects a useful user workflow.

## References

- [README.md](../../README.md)
- [ROADMAP.md](../../ROADMAP.md)
