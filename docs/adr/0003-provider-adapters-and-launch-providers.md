# ADR-0003: Provider adapters and launch providers

- Status: Accepted
- Date: 2026-10-23
- Deciders: Thais maintainers
- Related: ROADMAP M3, README decisions

## Context

Providers differ in tools, thinking blocks, usage, errors, and streaming. Forcing every provider through one shape loses control; writing an adapter for every provider is too much for v1. The launch set must be small enough to contract-test.

## Decision

- Native adapters for OpenAI and Anthropic. Anthropic uses the native Messages API, including thinking blocks; it is not forced through the OpenAI-compatible shape.
- One generic `openai-compatible` adapter covers OpenRouter, DeepSeek, and later presets.
- Launch-validated providers: OpenAI, Anthropic, OpenRouter, and DeepSeek.
- Qwen/DashScope, Z.ai/GLM, vLLM, and Ollama presets ship after launch; local models are not a v1 blocker.
- LiteLLM is optional and governed by [ADR-0004](0004-litellm.md).
- Presets are defaults, not contracts. Users may override base URL, headers, models, capabilities, and `extra`.
- Provider configuration contains no secrets; auth is a reference to the keyring or environment.

## Consequences

- Four providers need contract tests and SSE fixtures at launch.
- The generic adapter covers the long tail without new code per provider.
- Native control is preserved for the two largest APIs.
- Additional presets are additive and do not change the session layer.

## Alternatives considered

- Only OpenAI and Anthropic: less reach.
- All presets at launch: too much validation scope.
- Anthropic through the OpenAI-compatible path: loses native thinking and tool semantics.

## References

- [README.md](../../README.md)
- [ROADMAP.md](../../ROADMAP.md)
