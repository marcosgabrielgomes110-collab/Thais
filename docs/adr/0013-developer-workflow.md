# ADR-0013: Developer workflow and install prefix

- Status: Accepted
- Date: 2026-10-23
- Deciders: Thais maintainers
- Related: ROADMAP Developer workflow, Makefile

## Context

A single developer needs one reliable entry point for local work and CI. Install should not require root by default. Commands must work on macOS and BSD as well as Linux.

## Decision

- The `Makefile` is the single entry point. Targets: `setup`, `fmt`, `fmt-check`, `lint`, `test`, `build`, `run`, `install`, `uninstall`, `clean`, `ci`, `audit`, `doctor`, `help`.
- `make` runs `setup`, `check`, and `build`; `make check` runs `lint` and `test`; `make ci` is the single source of truth for CI.
- `PREFIX ?= $(HOME)/.local` and `BINDIR ?= $(PREFIX)/bin` are the defaults.
- Use `mkdir -p` and `install -m 0755`; do not use `install -D`.
- Do not put `sudo` in the Makefile.
- `make uninstall` removes the binary only and preserves configuration, credentials, sessions, and memory.
- Use `.editorconfig` with tabs for the Makefile and pin the toolchain with `rust-toolchain.toml`.
- Windows uses Git Bash or WSL for v1; a native equivalent is later.

## Consequences

- Consistent local and CI behavior.
- User-local install needs no root.
- Windows-native install work is deferred.
- The Makefile and docs must stay in sync.

## Alternatives considered

- `just` or `Taskfile`: viable, but the Makefile is already the chosen entry point and is universally available.
- `/usr/local` default: needs root and is less friendly for a single-user tool.

## References

- [README.md](../../README.md)
- [ROADMAP.md](../../ROADMAP.md)
