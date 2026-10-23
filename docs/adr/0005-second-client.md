# ADR-0005: Second client is the VS Code extension

- Status: Accepted
- Date: 2026-10-23
- Deciders: Thais maintainers
- Related: ROADMAP M7, README invariant 12

## Context

v1 needs a second client to prove the Agent API. A web UI and a VS Code extension are both plausible; building both delays the release and expands the security surface.

## Decision

- The VS Code extension is the v1 second client, delivered in M7.
- The web UI is a post-v1 candidate, not a v1 deliverable.
- Both, when built, use the same versioned Agent API through the SDK or protocol; neither imports core packages or reads storage.
- The daemon/WebSocket transport provides multi-client sessions; stdio remains single-client.
- No second client starts before M6.

## Consequences

- M7 focuses on one client and one test matrix.
- The extension fits the coding workflow and reuses the daemon.
- Web UI work is deferred, not canceled.
- The protocol and SDK must be stable before M7.

## Alternatives considered

- Web UI first: broader demo, but more auth, hosting, and UI surface.
- Both in M7: higher risk of slipping v1.

## References

- [README.md](../../README.md)
- [ROADMAP.md](../../ROADMAP.md)
