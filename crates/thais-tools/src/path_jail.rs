//! Path confinement for read-only tools.

use std::path::{Path, PathBuf};

use crate::error::ToolError;

/// Resolves tool paths inside a canonical workspace root.
#[derive(Debug, Clone)]
pub struct PathJail {
    canonical_root: PathBuf,
}

impl PathJail {
    /// Build a jail from a workspace root.
    pub fn new(root: &Path) -> Result<Self, ToolError> {
        let canonical_root = std::fs::canonicalize(root).map_err(|source| ToolError::Io {
            path: root.to_path_buf(),
            source,
        })?;
        Ok(Self { canonical_root })
    }

    /// Canonical workspace root.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.canonical_root
    }

    /// Resolve a requested path and reject escapes and symlink escapes.
    pub fn resolve(&self, requested: &str) -> Result<PathBuf, ToolError> {
        let requested = requested.trim();
        let path = if requested.is_empty() || requested == "." {
            self.canonical_root.clone()
        } else {
            let path = Path::new(requested);
            if path.is_absolute() {
                path.to_path_buf()
            } else {
                self.canonical_root.join(path)
            }
        };

        let canonical = if path.exists() {
            std::fs::canonicalize(&path).map_err(|source| ToolError::Io {
                path: path.clone(),
                source,
            })?
        } else {
            let parent = path.parent().ok_or_else(|| {
                ToolError::InvalidArgs(format!("path has no parent: {}", path.display()))
            })?;
            let parent = std::fs::canonicalize(parent).map_err(|source| ToolError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
            let name = path.file_name().ok_or_else(|| {
                ToolError::InvalidArgs(format!("path has no file name: {}", path.display()))
            })?;
            parent.join(name)
        };

        if !canonical.starts_with(&self.canonical_root) {
            return Err(ToolError::Escape(canonical));
        }
        Ok(canonical)
    }

    /// Display a path relative to the workspace root.
    #[must_use]
    pub fn display(&self, path: &Path) -> String {
        path.strip_prefix(&self.canonical_root)
            .unwrap_or(path)
            .display()
            .to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_parent_and_absolute_escapes() {
        let temp = tempfile::tempdir().expect("temp");
        let jail = PathJail::new(temp.path()).expect("jail");
        assert!(jail.resolve("../outside").is_err());
        assert!(jail.resolve("/etc/passwd").is_err());
        assert!(jail.resolve("inside.txt").is_ok());
    }
}
