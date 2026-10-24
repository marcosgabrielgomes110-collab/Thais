//! Shadow-Git snapshots for mutating actions.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use thiserror::Error;

/// One pre-action snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    /// Commit id in the shadow repository.
    pub commit: String,
}

/// Manages a shadow Git repository for one workspace/session.
#[derive(Debug, Clone)]
pub struct SnapshotManager {
    workspace: PathBuf,
    git_dir: PathBuf,
}

impl SnapshotManager {
    /// Create the shadow repository if it does not exist.
    pub fn new(workspace: &Path, base: &Path) -> Result<Self, SnapshotError> {
        let workspace = std::fs::canonicalize(workspace).map_err(|source| SnapshotError::Io {
            path: workspace.to_path_buf(),
            source,
        })?;
        std::fs::create_dir_all(base).map_err(|source| SnapshotError::Io {
            path: base.to_path_buf(),
            source,
        })?;
        let git_dir = base.join("shadow.git");
        if !git_dir.exists() {
            let status = Command::new("git")
                .args(["init", "--bare", "-q"])
                .arg(&git_dir)
                .status()
                .map_err(|error| SnapshotError::Git(format!("git init: {error}")))?;
            if !status.success() && !git_dir.join("HEAD").exists() {
                return Err(SnapshotError::Git("git init failed".to_owned()));
            }
        }
        Ok(Self { workspace, git_dir })
    }

    /// Snapshot the current workspace state.
    pub fn create(&self) -> Result<Snapshot, SnapshotError> {
        self.git(&["add", "-A"])?;
        let status = self
            .git(&["commit", "--allow-empty", "-qm", "snapshot"])?
            .status;
        if !status.success() {
            return Err(SnapshotError::Git("git commit failed".to_owned()));
        }
        let output = self.git(&["rev-parse", "HEAD"])?;
        let commit = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        if commit.is_empty() {
            return Err(SnapshotError::Git("empty snapshot commit".to_owned()));
        }
        Ok(Snapshot { commit })
    }

    /// Restore only paths that changed since the snapshot. Returns the paths
    /// that were restored or removed.
    pub fn restore(&self, snapshot: &Snapshot) -> Result<Vec<String>, SnapshotError> {
        let pre = self.tree_files(&snapshot.commit)?;
        let mut changed = self.diff_files(&snapshot.commit)?;
        changed.extend(self.untracked_files()?);
        changed.sort();
        changed.dedup();

        let mut restored = Vec::new();
        for path in changed {
            if path.is_empty() || path.contains("..") {
                continue;
            }
            if pre.contains(&path) {
                let status = self
                    .git(&["checkout", &snapshot.commit, "--", &path])?
                    .status;
                if status.success() {
                    restored.push(path);
                }
            } else {
                let full = self.workspace.join(&path);
                if full.is_file() || full.is_symlink() {
                    std::fs::remove_file(&full).map_err(|source| SnapshotError::Io {
                        path: full.clone(),
                        source,
                    })?;
                    self.prune_empty_parents(&full);
                    restored.push(path);
                }
            }
        }
        Ok(restored)
    }

    fn tree_files(&self, commit: &str) -> Result<HashSet<String>, SnapshotError> {
        let output = self.git(&["ls-tree", "-r", "--name-only", "-z", commit])?;
        Ok(split_nul(&output.stdout).into_iter().collect())
    }

    fn diff_files(&self, commit: &str) -> Result<Vec<String>, SnapshotError> {
        let output = self.git(&["diff", "--name-only", "--no-renames", "-z", commit, "--"])?;
        Ok(split_nul(&output.stdout))
    }

    fn untracked_files(&self) -> Result<Vec<String>, SnapshotError> {
        let output = self.git(&["ls-files", "--others", "--exclude-standard", "-z"])?;
        Ok(split_nul(&output.stdout))
    }

    fn git(&self, args: &[&str]) -> Result<std::process::Output, SnapshotError> {
        let output = Command::new("git")
            .args(args)
            .current_dir(&self.workspace)
            .env("GIT_DIR", &self.git_dir)
            .env("GIT_WORK_TREE", &self.workspace)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_OPTIONAL_LOCKS", "0")
            .env("GIT_AUTHOR_NAME", "thais")
            .env("GIT_AUTHOR_EMAIL", "thais@localhost")
            .env("GIT_COMMITTER_NAME", "thais")
            .env("GIT_COMMITTER_EMAIL", "thais@localhost")
            .output()
            .map_err(|error| SnapshotError::Git(format!("git: {error}")))?;
        if !output.status.success() {
            return Err(SnapshotError::Git(format!(
                "git {}: {}",
                args.join(" "),
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        Ok(output)
    }

    fn prune_empty_parents(&self, path: &Path) {
        let mut parent = path.parent();
        while let Some(directory) = parent {
            if directory == self.workspace {
                break;
            }
            if std::fs::remove_dir(directory).is_err() {
                break;
            }
            parent = directory.parent();
        }
    }
}

/// Snapshot failures.
#[derive(Debug, Error)]
pub enum SnapshotError {
    /// I/O failure.
    #[error("snapshot I/O error at {path}: {source}")]
    Io {
        /// Path involved.
        path: PathBuf,
        /// Underlying error.
        #[source]
        source: std::io::Error,
    },
    /// Git failure.
    #[error("snapshot git error: {0}")]
    Git(String),
}

fn split_nul(bytes: &[u8]) -> Vec<String> {
    bytes
        .split(|byte| *byte == 0)
        .filter(|part| !part.is_empty())
        .map(|part| String::from_utf8_lossy(part).into_owned())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_restores_modified_added_and_deleted_files() {
        let temp = tempfile::tempdir().expect("temp");
        let workspace = temp.path().join("repo");
        std::fs::create_dir_all(&workspace).expect("workspace");
        std::fs::write(workspace.join("a.txt"), "one\n").expect("a");
        std::fs::write(workspace.join("c.txt"), "three\n").expect("c");

        let manager =
            SnapshotManager::new(&workspace, &temp.path().join("snapshots")).expect("manager");
        let snapshot = manager.create().expect("snapshot");

        std::fs::write(workspace.join("a.txt"), "two\n").expect("modify");
        std::fs::write(workspace.join("b.txt"), "new\n").expect("add");
        std::fs::remove_file(workspace.join("c.txt")).expect("delete");

        let restored = manager.restore(&snapshot).expect("restore");
        assert!(restored.iter().any(|path| path == "a.txt"));
        assert!(restored.iter().any(|path| path == "b.txt"));
        assert!(restored.iter().any(|path| path == "c.txt"));
        assert_eq!(
            std::fs::read_to_string(workspace.join("a.txt")).expect("a"),
            "one\n"
        );
        assert_eq!(
            std::fs::read_to_string(workspace.join("c.txt")).expect("c"),
            "three\n"
        );
        assert!(!workspace.join("b.txt").exists());
    }
}
