# Thais development tasks
SHELL := /bin/sh

BIN ?= thais
PREFIX ?= $(HOME)/.local
BINDIR ?= $(PREFIX)/bin
ARGS ?=
TARGET ?= target/release/$(BIN)
DIST_DIR ?= dist
PROVIDER ?= $(if $(filter cohere,$(MAKECMDGOALS)),cohere,fake)
MODEL ?=
WORKSPACE ?=

TUI_ARGS := --provider $(PROVIDER)
ifneq ($(MODEL),)
TUI_ARGS += --model "$(MODEL)"
endif
ifneq ($(WORKSPACE),)
TUI_ARGS += --workspace "$(WORKSPACE)"
endif

.PHONY: all setup tools check fmt fmt-check lint test boundary build run tui fake cohere install uninstall clean ci audit doctor help

all: setup check build

setup:
	@command -v cargo >/dev/null 2>&1 || { echo "cargo not found; install Rust from https://rustup.rs"; exit 1; }
	@if command -v rustup >/dev/null 2>&1; then \
		rustup component add rustfmt clippy; \
	else \
		cargo fmt --version >/dev/null 2>&1 || { echo "rustfmt not found; install the rustfmt component or package"; exit 1; }; \
		cargo clippy --version >/dev/null 2>&1 || { echo "clippy not found; install the clippy component or package"; exit 1; }; \
	fi
	@command -v cargo-nextest >/dev/null 2>&1 || echo "cargo-nextest not found; make test will use cargo test (run 'make tools' to install)"
	cargo fetch

tools:
	@command -v cargo >/dev/null 2>&1 || { echo "cargo not found; install Rust from https://rustup.rs"; exit 1; }
	cargo install cargo-nextest --locked
	cargo install cargo-audit --locked
	cargo install cargo-deny --locked

check: lint test boundary

fmt:
	cargo fmt --all

fmt-check:
	cargo fmt --all -- --check

lint: fmt-check
	cargo clippy --workspace --all-targets --all-features -- -D warnings

test:
	@if command -v cargo-nextest >/dev/null 2>&1; then \
		cargo nextest run --workspace; \
	else \
		cargo test --workspace; \
	fi

boundary:
	@if cargo tree -p thais-tui -e normal | grep -q 'thais-core'; then \
		echo "architecture violation: thais-tui must not depend on thais-core"; \
		exit 1; \
	fi

build:
	cargo build --release --locked

run:
	cargo run -p thais-cli -- $(ARGS)

tui:
	cargo run -p thais-cli -- tui $(TUI_ARGS)

# Shorthand goals for `make tui cohere` / `make tui fake`.
fake cohere:
	@:

install: build
	mkdir -p "$(BINDIR)"
	install -m 0755 "$(TARGET)" "$(BINDIR)/$(BIN)"
	@echo "installed $(BINDIR)/$(BIN)"

uninstall:
	rm -f "$(BINDIR)/$(BIN)"
	@echo "removed $(BINDIR)/$(BIN); config, credentials, sessions, and memory were preserved"

clean:
	cargo clean
	rm -rf "$(DIST_DIR)"

ci: setup check build

audit:
	@command -v cargo-audit >/dev/null 2>&1 || { echo "install cargo-audit: cargo install cargo-audit --locked"; exit 1; }
	@command -v cargo-deny >/dev/null 2>&1 || { echo "install cargo-deny: cargo install cargo-deny --locked"; exit 1; }
	cargo audit
	cargo deny check

doctor: build
	"$(TARGET)" doctor

help:
	@printf '%s\n' \
		'Thais development tasks' \
		'' \
		'  make              setup + check + build' \
		'  make setup        check the Rust toolchain and fetch dependencies' \
		'  make tools        install cargo-nextest, cargo-audit, and cargo-deny' \
		'  make check        lint + test + architecture boundary' \
		'  make fmt          format code' \
		'  make lint         fmt check + clippy' \
		'  make test         run tests (nextest, or cargo test)' \
		'  make boundary     fail if thais-tui depends on thais-core' \
		'  make build        release build' \
		'  make run          run with ARGS="..."' \
		'  make tui          run the TUI (PROVIDER=fake by default)' \
		'  make tui PROVIDER=cohere   run the TUI with Cohere' \
		'  make tui cohere            shorthand for the same' \
		'  make tui PROVIDER=cohere MODEL=command-a-plus-05-2026 WORKSPACE=.' \
		'  make install      install to $(BINDIR)' \
		'  make uninstall    remove the installed binary' \
		'  make clean        remove build output' \
		'  make ci           setup + check + build' \
		'  make audit        cargo audit + cargo deny' \
		'  make doctor       run thais doctor' \
		'  make help         show this help'
