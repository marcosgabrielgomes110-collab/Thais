# Thais Agent API v1

- Status: M0 protocol
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
| `session.create` | `workspace`, `mode` (`plan`/`build`), `model` optional | `session` |
| `session.send` | `session_id`, `input`, `turn_id` optional | `turn_id` |
| `session.cancel` | `session_id`, `turn_id` optional | `cancelled` |
| `session.approve` | `session_id`, `approval_id`, `decision` (`allow`/`deny`) | `resolved` |
| `session.subscribe` | `session_id`, `from_seq` optional | `subscription_id`, `from_seq` |

`session_id` is present from day one even when only one session exists.
`session.create` uses the provider's first model when `model` is omitted.

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
  approvals return `resolved: false`.

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
| `turn_completed` | `turn_id`, `usage.input_tokens`, `usage.output_tokens` |
| `turn_cancelled` | `turn_id` |
| `error` | `turn_id`, `message` |

M0 emits the session, turn, model, reasoning, approval, completion, cancellation,
and error events. M1 adds read tools. M2 adds snapshots, rollback, verification,
write/edit/patch, bash, and git read tools.

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
