//! Path confinement for read-only tools.

use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};

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
            if path
                .components()
                .any(|component| matches!(component, Component::ParentDir))
            {
                return Err(ToolError::Escape(path.to_path_buf()));
            }
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
            let mut ancestor = path.as_path();
            let mut suffix: Vec<OsString> = Vec::new();
            while !ancestor.exists() {
                let name = ancestor.file_name().ok_or_else(|| {
                    ToolError::InvalidArgs(format!("path has no file name: {}", path.display()))
                })?;
                suffix.push(name.to_os_string());
                ancestor = ancestor.parent().ok_or_else(|| {
                    ToolError::InvalidArgs(format!(
                        "path has no existing ancestor: {}",
                        path.display()
                    ))
                })?;
            }
            let mut canonical =
                std::fs::canonicalize(ancestor).map_err(|source| ToolError::Io {
                    path: ancestor.to_path_buf(),
                    source,
                })?;
            for name in suffix.iter().rev() {
                canonical.push(name);
            }
            canonical
        };

        if !canonical.starts_with(&self.canonical_root) {
            return Err(ToolError::Escape(canonical));
        }
        Ok(canonical)
    }

    /// Resolve a mutation target without following its final symlink.
    ///
    /// Parent directories are canonicalized so symlinked parents cannot escape
    /// the jail. The final component is preserved so deleting or moving a
    /// symlink affects the link, not its target.
    pub fn resolve_mutation(&self, requested: &str) -> Result<PathBuf, ToolError> {
        let requested = requested.trim();
        if requested.is_empty() || requested == "." {
            return Err(ToolError::InvalidArgs(
                "the workspace root is protected".to_owned(),
            ));
        }
        let path = Path::new(requested);
        if path
            .components()
            .any(|component| matches!(component, Component::ParentDir))
        {
            return Err(ToolError::Escape(path.to_path_buf()));
        }
        let path = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.canonical_root.join(path)
        };
        if !path.starts_with(&self.canonical_root) {
            return Err(ToolError::Escape(path));
        }
        let parent = path.parent().ok_or_else(|| {
            ToolError::InvalidArgs(format!("path has no parent: {}", path.display()))
        })?;
        let name = path.file_name().ok_or_else(|| {
            ToolError::InvalidArgs(format!("path has no file name: {}", path.display()))
        })?;
        let parent = std::fs::canonicalize(parent).map_err(|source| ToolError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
        if !parent.starts_with(&self.canonical_root) {
            return Err(ToolError::Escape(parent));
        }
        let resolved = parent.join(name);
        self.ensure_mutable(&resolved)?;
        Ok(resolved)
    }

    /// Reject protected paths for mutations.
    pub fn ensure_mutable(&self, path: &Path) -> Result<(), ToolError> {
        if !path.starts_with(&self.canonical_root) {
            return Err(ToolError::Escape(path.to_path_buf()));
        }
        if path == self.canonical_root {
            return Err(ToolError::InvalidArgs(
                "the workspace root is protected".to_owned(),
            ));
        }
        let relative = path
            .strip_prefix(&self.canonical_root)
            .map_err(|_| ToolError::Escape(path.to_path_buf()))?;
        if relative
            .components()
            .any(|component| matches!(component.as_os_str().to_str(), Some(".git" | ".thais")))
        {
            return Err(ToolError::InvalidArgs(format!(
                "{} is a protected path",
                self.display(path)
            )));
        }
        Ok(())
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

    #[test]
    fn mutations_reject_root_git_and_thais() {
        let temp = tempfile::tempdir().expect("temp");
        std::fs::create_dir_all(temp.path().join(".git")).expect("git");
        std::fs::create_dir_all(temp.path().join(".thais")).expect("thais");
        std::fs::create_dir_all(temp.path().join("src")).expect("src");
        let jail = PathJail::new(temp.path()).expect("jail");
        assert!(jail.resolve_mutation(".").is_err());
        assert!(jail.resolve_mutation("").is_err());
        assert!(jail.resolve_mutation(".git/config").is_err());
        assert!(jail.resolve_mutation(".thais/config.toml").is_err());
        assert!(jail.resolve_mutation("src/main.rs").is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn mutation_resolution_does_not_follow_the_final_symlink() {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().expect("temp");
        let outside = tempfile::tempdir().expect("outside");
        std::fs::write(outside.path().join("target.txt"), "outside").expect("target");
        symlink(
            outside.path().join("target.txt"),
            temp.path().join("link.txt"),
        )
        .expect("link");
        let jail = PathJail::new(temp.path()).expect("jail");
        let resolved = jail.resolve_mutation("link.txt").expect("resolve");
        assert_eq!(resolved, temp.path().join("link.txt"));
        assert_eq!(
            std::fs::read_to_string(outside.path().join("target.txt")).expect("read"),
            "outside"
        );
    }
}
