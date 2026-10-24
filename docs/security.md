# Security model and residual risks

## Trust boundaries

- Model output, repository content, tool output, retrieved history, MCP responses, and memory are untrusted data.
- The Permission Gate authorizes every tool call, subagent task, and memory write.
- Plan mode is read-only; plan approval is the only transition to build mode.
- Project `AGENTS.md` and `<repo>/.thais/config.toml` cannot override safety, providers, auth, permissions, verifier commands, or sandbox policy.

## Process sandbox

The runtime auto-detects a Linux backend:

- `bwrap` (bubblewrap) with `--unshare-all`, read-only root, writable workspace, private `/proc` and `/dev`, and no network.
- If `bwrap` is unavailable, the `thais-sandbox` helper applies Landlock filesystem rules (read-only root, read-write workspace), denies TCP connect/bind, and applies a seccomp denylist for mount, ptrace, bpf, keyctl, kernel module, reboot, swap, and syslog syscalls.

`THAIS_SANDBOX` controls behavior:

- `auto` (default): use the backend when present; otherwise fall back to unsandboxed execution.
- `off`: disable the backend.
- `required`: refuse execution when no backend is available.

`bash` remains `ask` in all modes. The path jail is not an OS sandbox.

## Known residual risks

- Linux without `bwrap`: no OS sandbox; `bash` is still gated but can read/write outside the workspace and reach the network.
- The Linux sandbox uses namespaces and/or Landlock plus a seccomp denylist; it is not a complete syscall allowlist.
- macOS and Windows are post-v1; their sandbox backends are not part of the Linux v1.0 gate.
- Landlock is not available on kernels older than 5.13; fallback remains unsandboxed but gated.
- Shell commands may spawn descendants; process-group cleanup is best-effort after timeout/cancel.
- Snapshots honor `.gitignore`; ignored files and bash changes are best-effort.
- Tool output is redacted before it reaches the model and Event Log, but a secret that matches no pattern can still pass.
- Pending approvals are durable, but an interrupted in-flight turn is not replayed.

## Verification

- `make check` includes fmt, clippy, tests, and the architecture boundary.
- Tests cover redaction before model/log, sandbox fallback, required-sandbox refusal, gate behavior, and lease release.
- CI runs Linux tests and dependency audit.
