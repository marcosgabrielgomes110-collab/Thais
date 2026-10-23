# Rollback spike: capturing bash changes

- Status: M0 spike complete; implementation decision for M2
- Date: 2026-10-23
- Scope: code-changing `bash` actions; Thais-owned changes only
- Related: ROADMAP M0, M2, M6; README invariants 5 and 10

## Question

How should Thais capture the state of a workspace before a `bash` action so a
failed verification can restore Thais-owned changes without discarding user
work?

The non-negotiable constraint: the workspace path jail is not an OS sandbox.
Bash can modify anything the user can modify. Rollback for bash is therefore
explicitly **best-effort**, and the implementation must say when it cannot
restore a path.

## What a snapshot must cover

| Change | Shadow Git | Pre/post hashes + backup | Notes |
|---|---|---|---|
| Create file | Yes | Yes | New file is not in the pre-state; delete on restore. |
| Modify file | Yes | Yes | Backup needed for restore. |
| Delete file | Yes | Yes | Pre-state object/backup is required. |
| Rename/move | Yes (delete + add) | Yes (delete + add) | Git detects renames heuristically; restore is content-based. |
| Replace file with directory | Mostly | Yes with care | Path-type transitions need explicit tests. |
| Binary file | Yes | Yes | Copy cost proportional to changed bytes. |
| Large file | Yes, with cost | Yes, with cost | Needs a size cap and a clear failure mode. |
| Executable bit | Yes | Partial | Git records the executable bit, not the full mode. |
| Arbitrary permissions | No | Partial | `0600`/`0640` differences may be lost. |
| Symlink | Yes | Partial | Symlink target and link-vs-file need explicit handling. |
| Hardlink | No | No | Link identity is not preserved. |
| xattrs / ACLs | No | No | Out of scope for M2; document as residual risk. |
| Ignored/untracked files | Only with force-add | Yes | Honoring `.gitignore` would miss changes bash made. |
| Files outside workspace | No | No | Explicit residual risk. |
| Network/process side effects | No | No | Cannot be rolled back. |
| Concurrent external edits | Racy | Racy | Snapshot is not a transaction with the filesystem. |

## Options

### A. Shadow Git repository

Use a dedicated bare repository under
`~/.local/share/thais/snapshots/<session>/shadow.git` and a separate
`GIT_DIR`/`GIT_WORK_TREE`. Before a bash action:

1. `git add -A -f` in the workspace with the shadow `GIT_DIR`.
2. Commit with a deterministic message and a snapshot id.
3. Record the commit id and the event id.

Restore means checking out the pre-action commit into a temporary directory,
then restoring only paths that Thais can prove it changed. The project's real
`.git` directory must be excluded from the shadow work tree.

**Strengths:** content-addressed storage, deduplication, deletions/renames,
binary support, symlinks, executable bit, easy diffing, well-understood
implementation.

**Weaknesses:** a full `git add -A` scan on every bash action; ignored files
require `-f` or a separate policy; Git does not preserve xattrs/ACLs/hardlinks;
large files and very large repositories need caps; Git must be available or
the snapshot must fail closed.

### B. Pre/post hash manifest plus changed-file backups

Before a bash action, walk the workspace, hash each file, and copy the
pre-state of every file into a content-addressed backup store. After the
action, walk again and diff the manifests. Restore copies the pre-state back
and removes files that did not exist before.

**Strengths:** predictable cost, no Git dependency, easy to cap by file size
or count, straightforward “only changed paths” restore.

**Weaknesses:** a full walk still touches every file; backup storage grows with
changed bytes; implementing renames, path-type transitions, symlinks, and
permission bits correctly is more code than using Git; ignored files must be
included explicitly.

### C. Hybrid

Use the hash manifest for detection and the shadow Git object store (or a
plain content-addressed backup directory) for restore. This is a likely M2
shape if shadow Git performance is unacceptable on large repositories.

## Sample measurements

Machine: Linux x86_64, warm cache, 2,001 files, about 28 MB, one 20 MB file.
Each git snapshot included a small change and `git add -A` plus commit. Each
hash pass used `find`, `sort`, and `sha256sum`.

| Operation | Samples | Snapshot size |
|---|---|---|
| Shadow Git | 168 ms, 105 ms, 131 ms | 8.6 MB |
| Hash manifest only | 34 ms, 34 ms, 28 ms | 164 KB |

The hash manifest is faster and much smaller, but it only detects changes; a
real restore also needs a copy of changed pre-state bytes. On this sample, a
changed 20 MB binary would cost roughly 20 MB of backup either way.

These numbers are not a benchmark suite. M2 must repeat them on a large
repository (100k+ files), a repository with many ignored build artifacts, and
a repository with large binaries.

## Recommendation for M2

Start with **shadow Git** for correctness and implementation speed, with these
guardrails:

- snapshot before the bash action, never after;
- exclude the real `.git` directory and Thais data directories;
- force-add ignored files so bash changes to them are visible;
- cap total files and bytes; if the cap is exceeded, return `unavailable` and
  keep bash `ask`-only for that action;
- run Git with a timeout and no interactive prompts or hooks
  (`core.hooksPath=/dev/null`, `GIT_TERMINAL_PROMPT=0`);
- record snapshot id, commit id, event id, and the list of restored paths;
- restore only paths Thais can attribute to the action, and report paths that
  could not be restored;
- mark any partial restore `unverified` and stop the loop for escalation.

Keep the hash manifest as a verification layer and as the fallback when Git is
missing or the repository exceeds the caps.

Do not claim complete rollback. The following remain outside the guarantee:

- files outside the workspace;
- network calls, spawned daemons, and external services;
- hardlinks, xattrs, ACLs, and arbitrary permission bits;
- changes made by another process during snapshot or restore;
- storage exhaustion or a snapshot that fails before the action starts.

## M2 acceptance tests for the chosen implementation

1. Create, modify, delete, and rename a text file; restore the exact pre-state.
2. Replace a file with a directory and the reverse.
3. Restore a binary file and a file larger than the configured cap behavior.
4. Restore a symlink and an executable-bit change.
5. Capture a change to a file ignored by the project's `.gitignore`.
6. Report a path outside the workspace that cannot be restored.
7. Simulate a snapshot failure: bash must not run, and the gate must report why.
8. Simulate a partial restore: the action is marked `unverified` and escalates.
9. Run on a repository with 100k files and record snapshot latency and size.
10. Run with Git unavailable and verify the documented fallback.

## Decision

Adopt shadow Git for the M2 implementation, keep the hash manifest for
verification and fallback, and publish the best-effort limits in the release
notes. Revisit the hybrid design if the large-repository benchmark shows that
snapshot latency blocks normal use.
