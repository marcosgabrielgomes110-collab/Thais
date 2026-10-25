# Changelog

## Unreleased — v1.0.0 Linux

- Added `delete`, `move`, and `mkdir` with path-jail protection, previews, and approval.
- Added `memory_read` and `memory_search`.
- Replaced the permission gate's hardcoded tool list with the registry catalog.
- Added `git_branch`, `git_checkout`, and `git_stash`.
- Added `secret_scan` and automatic secret scanning before `git_commit`.
- Added background jobs (`bash_start`, `bash_logs`, `bash_stop`) with bounded logs, process-group cleanup, and approval separate from workspace mutation.
- Added structured `test` and `build` tools with compact diagnostics and full log paths.

- Added CycloneDX SBOM generation with `scripts/sbom.py`.
- Added GPG signing and release verification scripts.
- Added `cargo-fuzz` protocol/tool-argument targets and stable proptest coverage.
- Added a two-client WebSocket event test.
- Added a Linux performance smoke test with thresholds.
- Added and validated cargo-dist archives, checksums, and shell installers locally.
- Scoped v1.0 to Linux; macOS and Windows are post-v1.

## v0.8.0-m7

- Added the local WebSocket daemon with token authentication and multi-client events.
- Added `thais events --follow`.
- Added the VS Code extension scaffold.
- Added packaging metadata, Dockerfile, and release documentation.

## v0.7.0-m6

- Linux sandbox: bubblewrap namespaces, Landlock, and seccomp fallback.
- Tool-output redaction, idempotent event append, symlink-safe credentials.
- Prompt-injection and sandbox escape tests.

## v0.6.0-m5

- LSP, MCP stdio/HTTP, child sessions, subagents, and gated Git tools.

## v0.5.0-m4

- Approved `MEMORY.md`, context rebuild, and history search.

## v0.4.0-m3

- Sessions, workspace leases, providers, models, usage, and cost.

## v0.3.0-m2

- Safe build, snapshots, verifier, rollback, and personality.

## v0.2.0-m1

- Read-only agent loop and TUI.

## v0.1.0-m0

- Foundation, protocol, event log, CLI, and CI.
