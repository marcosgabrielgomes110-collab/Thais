# ADR-0001: Session lifecycle and workspace lease

- Status: Accepted
- Date: 2026-10-23
- Deciders: Thais maintainers
- Related: ROADMAP M3, README invariants 4 and 9

## Context

Thais needs more than one session per workspace: a user may explore in a read-only session while a build session runs, and sessions must survive restarts. Concurrent mutations in the same workspace would race on files, snapshots, and rollback state, so the runtime needs a cross-process rule for who may write.

## Decision

- Sessions are first-class; multiple sessions may exist per workspace.
- At most one mutating session per canonical workspace holds an exclusive lease stored in SQLite. The lease is acquired before a mutating turn, has a heartbeat and timeout, and can be stolen only with explicit user confirmation.
- Any number of read-only sessions may coexist with the lease holder.
- The Permission Gate checks the lease before allowing a mutation; the lease is not a UI-only concept.
- A read-only session must acquire the lease before it can switch to build mode.
- The TUI and API expose the lease holder and lease state.

## Consequences

- Concurrent edits and lost rollback state are prevented.
- Read-only exploration stays parallel.
- Stale leases need recovery and tests across processes.
- The lease table and heartbeat belong to the M3 schema; M0 should leave room for them.

## Alternatives considered

- One active session per workspace: simpler, but blocks useful read-only parallelism.
- No lease: unsafe under concurrent processes.
- Lease only in the TUI: bypassable by the CLI, daemon, or a second client.

## References

- [README.md](../../README.md)
- [ROADMAP.md](../../ROADMAP.md)
