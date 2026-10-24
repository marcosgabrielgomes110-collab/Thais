# Thais Agent API v1

- Status: M4 protocol
- Version: `1.0`
- Transport: JSON-RPC 2.0 over stdio; one JSON message per line (NDJSON)
- Boundary: all clients use this protocol through `thais-sdk`; no client imports core internals

`stdout` carries protocol messages only. Logs, progress, and diagnostics go to
`stderr`.

## Framing

Each line on `stdin`/`stdout` is one complete JSON-RPC message. A message must
not contain a raw newline; JSON string escaping keeps it on one line. Empty
lines are ignored. The server replies with a parse error for a malformed line.
Responses and notifications may interleave; clients route responses by `id`
and events by `method`.

WebSocket transport (M7) uses the same message objects with one JSON message
per WebSocket frame.

## Handshake

Request:

```json
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"client_name":"thais-tui","client_version":"0.1.0","protocol_version":"1.0","capabilities":{}}}
```

Response:

```json
{"jsonrpc":"2.0","id":1,"result":{"server_name":"thais","server_version":"0.1.0","protocol_version":"1.0","capabilities":{"sessions":true,"approvals":true,"fake_provider":true}}}
```

A client that sends a non-empty `protocol_version` different from the server's
version receives `-32600` (invalid request). The server exposes its version in
the result so the client can decide how to proceed.

## Methods

| Method | Params | Result |
|---|---|---|
| `initialize` | `client_name`, `client_version`, `protocol_version`, `capabilities` | `server_name`, `server_version`, `protocol_version`, `capabilities` |
| `session.create` | `workspace`, `name` optional, `mode` (`plan`/`build`), `model` optional, `steal_lease` optional | `session` |
| `session.send` | `session_id`, `input`, `turn_id` optional, `model` optional | `turn_id` |
| `session.cancel` | `session_id`, `turn_id` optional | `cancelled` |
| `session.approve` | `session_id`, `approval_id`, `decision` (`allow`/`deny`) | `resolved` |
| `session.subscribe` | `session_id`, `from_seq` optional | `subscription_id`, `from_seq` |
| `session.list` | `include_archived` optional | `sessions` |
| `session.get` | `session_id` | `session` |
| `session.rename` | `session_id`, `name` | `session` |
| `session.archive` | `session_id` | `session` |
| `session.delete` | `session_id` | `deleted` (soft delete) |
| `session.release_lease` | `session_id` | `released` |
| `session.resume` | `session_id` | `session`, `checkpoint`, `pending_approvals` |
| `session.set_model` | `session_id`, `role`, `model` | `session` |
| `models.list` | none | `models` |
| `history.search` | `query`, `session_id` optional, `workspace` optional, `limit` optional | `matches` (`event`, `session_name`, `workspace`) |

A session object includes `session_id`, `name`, `workspace`, `mode`, `model`,
`model_policy`, lifecycle `status`, timestamps, and cumulative
`input_tokens`, `output_tokens`, and `cost_usd`. `session_id` is present from
day one even when only one session exists.
`session.create` uses the provider's first model when `model` is omitted.
`session.create` and `session.send` accept an optional per-turn/session model.
`session.set_model` changes the default (`role: "default"`) or one role.
`session.delete` is a soft delete: the session disappears from lists but its
Event Log is retained. `session.archive` keeps it visible when
`include_archived` is true.

### Sequence semantics

- `session.send` is asynchronous. The response acknowledges the turn; the
  model output arrives as `session.event` notifications.
- `session.subscribe` with `from_seq` set replays events with `seq > from_seq`
  after the response. Omit `from_seq` for live-only.
- Each event has a monotonic `seq` per session, starting at 1.
- A client that reconnects can subscribe with its last seen `seq`.
- `session.cancel` cancels the active turn, or a specific `turn_id`. The runtime
  emits `turn_cancelled` when it observes the cancellation.
- `session.approve` resolves the pending approval. Unknown or already-resolved
  approvals return `resolved: false`. Pending approvals are stored in SQLite;
  `session.resume` re-surfaces them after a restart. The original in-flight turn
  is not replayed, so a pending approval is either resolved or explicitly
  re-driven by the client.
- `session.create` and `resume` acquire the workspace mutating lease in build
  mode. The lease is one-per-workspace across processes, expires after five
  minutes without renewal, and can be stolen only with `steal_lease: true`.
  Plan sessions never take it. A client can release it explicitly with
  `session.release_lease`; the TUI does this on exit and when switching sessions.
- Session checkpoints are written under the data directory at
  `sessions/<session_id>/checkpoint.md` with a machine-readable block,
  including the approved plan, derived tasks, compacted summary, and the last
  sequence covered by that summary.
- `history.search` queries the derived `events_fts` index and can filter by
  session and workspace. `events` remains the source of truth; rebuilding
  `events_fts` from it returns the same matches.

## Events

Events are sent as notifications:

```json
{"jsonrpc":"2.0","method":"session.event","params":{"event":{"event_id":"evt_...","session_id":"session_...","seq":1,"timestamp":"2026-10-23T00:00:00Z","kind":"session_created","payload":{}}}}
```

Envelope fields:

| Field | Meaning |
|---|---|
| `event_id` | Stable identifier; unique across the log |
| `session_id` | Session the event belongs to |
| `seq` | Monotonic per-session sequence, starting at 1 |
| `timestamp` | RFC 3339 timestamp |
| `kind` | Event type |
| `payload` | Event-specific object |

Event kinds in v1:

| Kind | Payload |
|---|---|
| `session_created` | `session` |
| `model_changed` | `role`, `model`, optional `reason` |
| `budget_warning` | `turn_id`, `budget`, `used`, `limit` |
| `context_rebuilt` | `turn_id`, `older_turns`, `kept_turns`, `last_compacted_seq`, `summary_chars` |
| `context_rebuild_failed` | `turn_id`, `message` |
| `memory_updated` | `turn_id`, `rule`, `diff` |
| `snapshot_created` | `turn_id`, `commit` |
| `rollback_completed` | `turn_id`, `paths` |
| `verification_started` | `turn_id`, `commands` |
| `verification_passed` | `turn_id` |
| `verification_failed` | `turn_id`, `output` |
| `verification_unavailable` | `turn_id`, `reason` |
| `turn_started` | `turn_id`, `input` |
| `model_delta` | `turn_id`, `text` |
| `reasoning_delta` | `turn_id`, `text` |
| `tool_call` | `turn_id`, `id`, `name`, `arguments` |
| `tool_result` | `turn_id`, `tool_call_id`, `name`, `content`, `is_error`, `diff` |
| `permission_requested` | `approval_id`, `action` |
| `permission_granted` | `turn_id`, `tool`, `summary` |
| `permission_denied` | `turn_id`, `tool`, `reason` |
| `permission_resolved` | `approval_id`, `decision` |
| `turn_completed` | `turn_id`, `model`, `usage.input_tokens`, `usage.output_tokens`, `cost_usd` |
| `turn_cancelled` | `turn_id` |
| `error` | `turn_id`, `message` |

M0 emits the session, turn, model, reasoning, approval, completion, cancellation,
and error events. M1 adds read tools. M2 adds snapshots, rollback, verification,
write/edit/patch, bash, and git read tools. M3 adds session lifecycle, model
policy changes, budget warnings, and durable pending approvals. M4 adds
approved project memory, context rebuilds, checkpoint summaries, and history
search.

## Errors

| Code | Meaning |
|---|---|
| `-32700` | Parse error |
| `-32600` | Invalid request or protocol version |
| `-32601` | Method not found |
| `-32602` | Invalid params, missing session, turn already active |
| `-32603` | Internal error, storage failure, provider failure |

Responses carry exactly one of `result` or `error`.

## M2 build flow

- `session.create` with `mode: "plan"` exposes read-only tools only. The gate
  denies mutations in plan mode.
- When a plan turn completes, the runtime emits `permission_requested` with an
  action of type `plan`. Approving it with `session.approve` switches the
  session to build mode; rejecting it keeps the session in plan mode.
- In build mode, `write`, `edit`, and `apply_patch` require approval. `bash`
  always requires approval until M6.
- Before every mutating action, the runtime writes a shadow-Git snapshot and
  emits `snapshot_created`.
- After a successful mutation, the configured verifier runs. A failure emits
  `verification_failed`, restores the snapshot, emits `rollback_completed`, and
  returns the failure to the model for a bounded repair attempt.
- `tool_result` includes the unified `diff` for mutations, so clients can show
  the change before and after approval.
- `git_status` and `git_diff` are read-only tools.

## M3 sessions and models

- `providers.toml` lives in the user config directory and contains only
  non-secret fields (`id`, `type`, `base_url`, `auth`, `models`, `extra`).
  Secrets are resolved from the environment, keyring, or the `0600` fallback.
- `models.toml` overrides catalog capabilities, context windows, aliases, and
  cost per million tokens. `models.list` returns the merged catalog.
- Model roles are `default`, `plan`, `build`, `explore`, `general`, and
  `compact`. Ordered fallback is visible as `model_changed` with
  `reason: "fallback"`.
- The main loop rejects models that do not support tool calling.
- Usage and cost are recorded on `turn_completed`; the TUI status bar shows the
  most recent input/output tokens and USD cost. A per-turn cost budget emits
  `budget_warning` at 80% and stops at 100%.

## M4 memory and continuity

- `MEMORY.md` lives at the project root. The model can call `memory_write` only
  in build mode; the permission gate asks for approval and shows a diff. The
  write is atomic, duplicate-resistant, secret-rejecting, and capped at 2,000
  estimated tokens.
- `MEMORY.md` is included below project and global `AGENTS.md` in the prompt and
  never overrides safety rules.
- After each turn the runtime writes a checkpoint. Before a context rebuild it
  writes a write-ahead checkpoint, then summarizes older turns and keeps the
  most recent four turns verbatim.
- Safety, `AGENTS.md`, the environment badge, and project memory are always
  rebuilt into the prompt. Rebuild success emits `context_rebuilt`; failure
  emits `context_rebuild_failed` and falls back to the checkpoint plus memory.
- `history.search` searches the Event Log with optional session and workspace
  filters.

## Security and trust

- Model output, tool output, and retrieved history are untrusted data.
- Project configuration is untrusted and cannot set providers, base URLs,
  credentials, permissions, verifier commands, or the sandbox level.
- Credentials are never part of protocol messages or events.
- Events are redacted before append. The protocol never receives the raw
  secret that triggered a redaction.

## Compatibility

`1.x` clients may ignore unknown event kinds and unknown payload fields.
Breaking changes require a protocol major version and a release note. The
stdio transport is single-client; multi-client per session is a daemon and
WebSocket feature (M7).
