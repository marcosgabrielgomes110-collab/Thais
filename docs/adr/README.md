# Architecture Decision Records

This directory records accepted decisions for Thais. The architecture invariants live in [README.md](../../README.md#invariants); the v1 decision index lives in [ROADMAP.md](../../ROADMAP.md#decisions).

An ADR is immutable once accepted. To change a decision, add a new ADR that supersedes the old one, update this index, and update the roadmap's Decisions table.

| ADR | Decision | Status |
|---|---|---|
| [0001](0001-session-lifecycle-and-workspace-lease.md) | Session lifecycle and workspace lease | Accepted |
| [0002](0002-model-policy-and-fallback.md) | Model policy and fallback | Accepted |
| [0003](0003-provider-adapters-and-launch-providers.md) | Provider adapters and launch providers | Accepted |
| [0004](0004-litellm.md) | LiteLLM is optional | Accepted |
| [0005](0005-second-client.md) | Second client is the VS Code extension | Accepted |
| [0006](0006-session-retention.md) | Session retention | Accepted |
| [0007](0007-telemetry.md) | No telemetry in v1 | Accepted |
| [0008](0008-sandbox-default.md) | Sandbox is enforced by default after M6 | Accepted |
| [0009](0009-credential-storage.md) | Credential storage and fallback | Accepted |
| [0010](0010-instruction-files-and-precedence.md) | Instruction files and precedence | Accepted |
| [0011](0011-environment-badge.md) | Environment badge privacy and refresh | Accepted |
| [0012](0012-tui-wide-tables.md) | Wide tables in the TUI | Accepted |
| [0013](0013-developer-workflow.md) | Developer workflow and install prefix | Accepted |

## Adding a decision

1. Copy the structure of an existing ADR.
2. Use the next four-digit number and a short kebab-case title.
3. Set the status to `Proposed` until the decision is accepted.
4. Link the ADR from the index and from the roadmap's Decisions table.
5. If the decision changes an earlier one, mark the earlier ADR `Superseded by ADR-NNNN` and link the replacement.
