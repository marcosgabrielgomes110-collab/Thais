//! Platform paths for configuration, data, state, cache, and per-project
//! files. All user-level paths live under the user's home directory and follow
//! the XDG base directory convention on Linux and the native convention on
//! macOS and Windows.

use std::ffi::OsString;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

use thiserror::Error;
use uuid::Uuid;

/// User-level directories used by Thais.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    config: PathBuf,
    data: PathBuf,
    state: PathBuf,
    cache: PathBuf,
}

impl Paths {
    /// Build a set of paths from explicit roots. Useful for tests and for
    /// callers that already resolved the platform directories.
    #[must_use]
    pub fn from_roots(
        config: impl Into<PathBuf>,
        data: impl Into<PathBuf>,
        state: impl Into<PathBuf>,
        cache: impl Into<PathBuf>,
    ) -> Self {
        Self {
            config: config.into(),
            data: data.into(),
            state: state.into(),
            cache: cache.into(),
        }
    }

    /// Resolve the user-level directories from the current environment.
    pub fn discover() -> Result<Self, PathError> {
        Self::from_lookup(|key| std::env::var_os(key))
    }

    /// Resolve directories using a custom environment lookup. Tests use this
    /// instead of mutating process-wide environment variables.
    pub fn from_lookup<F>(lookup: F) -> Result<Self, PathError>
    where
        F: Fn(&str) -> Option<OsString>,
    {
        #[cfg(all(unix, not(target_os = "macos")))]
        {
            let home = absolute_lookup(&lookup, "HOME").ok_or(PathError::HomeUnavailable)?;
            let config_base = xdg_or(&lookup, "XDG_CONFIG_HOME", home.join(".config"));
            let data_base = xdg_or(&lookup, "XDG_DATA_HOME", home.join(".local/share"));
            let state_base = xdg_or(&lookup, "XDG_STATE_HOME", home.join(".local/state"));
            let cache_base = xdg_or(&lookup, "XDG_CACHE_HOME", home.join(".cache"));
            return Ok(Self::from_roots(
                config_base.join("thais"),
                data_base.join("thais"),
                state_base.join("thais"),
                cache_base.join("thais"),
            ));
        }

        #[cfg(target_os = "macos")]
        {
            let home = absolute_lookup(&lookup, "HOME").ok_or(PathError::HomeUnavailable)?;
            let support = home.join("Library/Application Support/Thais");
            return Ok(Self::from_roots(
                support.join("config"),
                support.join("data"),
                home.join("Library/Logs/Thais"),
                home.join("Library/Caches/Thais"),
            ));
        }

        #[cfg(windows)]
        {
            let roaming = absolute_lookup(&lookup, "APPDATA")
                .or_else(|| {
                    absolute_lookup(&lookup, "USERPROFILE").map(|p| p.join("AppData/Roaming"))
                })
                .ok_or(PathError::HomeUnavailable)?;
            let local = absolute_lookup(&lookup, "LOCALAPPDATA")
                .or_else(|| {
                    absolute_lookup(&lookup, "USERPROFILE").map(|p| p.join("AppData/Local"))
                })
                .ok_or(PathError::HomeUnavailable)?;
            return Ok(Self::from_roots(
                roaming.join("Thais/config"),
                local.join("Thais/data"),
                local.join("Thais/state"),
                local.join("Thais/cache"),
            ));
        }

        #[allow(unreachable_code)]
        Err(PathError::HomeUnavailable)
    }

    /// Configuration directory (`~/.config/thais` on Linux).
    #[must_use]
    pub fn config_dir(&self) -> &Path {
        &self.config
    }

    /// Data directory (`~/.local/share/thais` on Linux).
    #[must_use]
    pub fn data_dir(&self) -> &Path {
        &self.data
    }

    /// State directory (`~/.local/state/thais` on Linux).
    #[must_use]
    pub fn state_dir(&self) -> &Path {
        &self.state
    }

    /// Cache directory (`~/.cache/thais` on Linux).
    #[must_use]
    pub fn cache_dir(&self) -> &Path {
        &self.cache
    }

    /// Path of the SQLite database.
    #[must_use]
    pub fn database_path(&self) -> PathBuf {
        self.data.join(crate::DATABASE_FILE)
    }

    /// Directory holding per-session checkpoints.
    #[must_use]
    pub fn sessions_dir(&self) -> PathBuf {
        self.data.join(crate::SESSIONS_DIR)
    }

    /// Directory for one session.
    #[must_use]
    pub fn session_dir(&self, session_id: &str) -> PathBuf {
        self.sessions_dir().join(session_id)
    }

    /// Runtime-owned checkpoint for a session.
    #[must_use]
    pub fn checkpoint_path(&self, session_id: &str) -> PathBuf {
        self.session_dir(session_id).join("checkpoint.md")
    }

    /// Directory holding rollback snapshots.
    #[must_use]
    pub fn snapshots_dir(&self) -> PathBuf {
        self.data.join(crate::SNAPSHOTS_DIR)
    }

    /// Non-secret configuration file inside the config directory.
    #[must_use]
    pub fn config_file(&self, name: &str) -> PathBuf {
        self.config.join(name)
    }

    /// Fallback credentials file. It is only written with mode `0600`.
    #[must_use]
    pub fn credentials_path(&self) -> PathBuf {
        self.config.join("credentials.toml")
    }

    /// Create every user-level directory with mode `0700` where supported.
    pub fn ensure(&self) -> Result<(), PathError> {
        for dir in [
            &self.config,
            &self.data,
            &self.state,
            &self.cache,
            &self.sessions_dir(),
            &self.snapshots_dir(),
        ] {
            create_private_dir(dir)?;
        }
        Ok(())
    }

    /// Ensure the directory for one session exists with mode `0700`.
    pub fn ensure_session_dir(&self, session_id: &str) -> Result<PathBuf, PathError> {
        let dir = self.session_dir(session_id);
        create_private_dir(&dir)?;
        Ok(dir)
    }
}

/// Paths inside a project workspace. Project content is untrusted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectPaths {
    root: PathBuf,
}

impl ProjectPaths {
    /// Build project paths from a workspace root.
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Workspace root.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Untrusted project configuration directory.
    #[must_use]
    pub fn thais_dir(&self) -> PathBuf {
        self.root.join(".thais")
    }

    /// Untrusted project configuration file.
    #[must_use]
    pub fn config_path(&self) -> PathBuf {
        self.thais_dir().join("config.toml")
    }

    /// Approved project memory file.
    #[must_use]
    pub fn memory_path(&self) -> PathBuf {
        self.thais_dir().join("MEMORY.md")
    }
}

/// Errors while resolving or creating user-level paths.
#[derive(Debug, Error)]
pub enum PathError {
    /// The home directory (or its native equivalent) is not available.
    #[error("could not determine the home directory")]
    HomeUnavailable,
    /// An I/O operation failed.
    #[error("I/O error at {path}: {source}")]
    Io {
        /// Path involved in the failure.
        path: PathBuf,
        /// Underlying error.
        #[source]
        source: std::io::Error,
    },
}

impl PathError {
    fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Self::Io {
            path: path.into(),
            source,
        }
    }
}

/// Write a private file atomically. On Unix the file is created with mode
/// `0600`; the temporary file is created next to the destination so the final
/// rename stays on the same filesystem.
pub fn write_private(path: &Path, contents: &[u8]) -> Result<(), PathError> {
    let parent = path.parent().ok_or_else(|| {
        PathError::io(
            path,
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "destination has no parent directory",
            ),
        )
    })?;
    create_private_dir(parent)?;
    if let Ok(metadata) = std::fs::symlink_metadata(path)
        && metadata.file_type().is_symlink()
    {
        return Err(PathError::io(
            path,
            std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "refusing to write through a symlink",
            ),
        ));
    }

    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("thais");
    let temporary = parent.join(format!(".{file_name}.tmp-{}", Uuid::new_v4()));

    let result =
        write_temporary(&temporary, contents).and_then(|()| rename_into_place(&temporary, path));
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

fn write_temporary(path: &Path, contents: &[u8]) -> Result<(), PathError> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }

    let mut file = options
        .open(path)
        .map_err(|source| PathError::io(path, source))?;
    file.write_all(contents)
        .map_err(|source| PathError::io(path, source))?;
    file.sync_all()
        .map_err(|source| PathError::io(path, source))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .map_err(|source| PathError::io(path, source))?;
    }
    Ok(())
}

fn rename_into_place(from: &Path, to: &Path) -> Result<(), PathError> {
    #[cfg(windows)]
    {
        // Windows does not replace an existing destination with rename. The
        // fallback is not atomic, but M0 validates Linux and macOS first.
        if to.exists() {
            std::fs::remove_file(to).map_err(|source| PathError::io(to, source))?;
        }
    }
    std::fs::rename(from, to).map_err(|source| PathError::io(to, source))
}

fn create_private_dir(path: &Path) -> Result<(), PathError> {
    std::fs::create_dir_all(path).map_err(|source| PathError::io(path, source))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
            .map_err(|source| PathError::io(path, source))?;
    }
    Ok(())
}

fn absolute_lookup<F>(lookup: &F, key: &str) -> Option<PathBuf>
where
    F: Fn(&str) -> Option<OsString>,
{
    lookup(key)
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
}

#[cfg(all(unix, not(target_os = "macos")))]
fn xdg_or<F>(lookup: &F, key: &str, default: PathBuf) -> PathBuf
where
    F: Fn(&str) -> Option<OsString>,
{
    absolute_lookup(lookup, key).unwrap_or(default)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derives_paths_from_roots() {
        let paths = Paths::from_roots("/cfg", "/data", "/state", "/cache");
        assert_eq!(paths.config_dir(), Path::new("/cfg"));
        assert_eq!(paths.database_path(), Path::new("/data/thais.db"));
        assert_eq!(
            paths.checkpoint_path("s-1"),
            Path::new("/data/sessions/s-1/checkpoint.md")
        );
        assert_eq!(paths.credentials_path(), Path::new("/cfg/credentials.toml"));
    }

    #[test]
    fn project_paths_are_relative_to_workspace() {
        let project = ProjectPaths::new("/work/repo");
        assert_eq!(project.root(), Path::new("/work/repo"));
        assert_eq!(
            project.config_path(),
            Path::new("/work/repo/.thais/config.toml")
        );
        assert_eq!(
            project.memory_path(),
            Path::new("/work/repo/.thais/MEMORY.md")
        );
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn linux_uses_home_and_xdg() {
        let paths = Paths::from_lookup(|key| match key {
            "HOME" => Some(OsString::from("/home/tester")),
            "XDG_CONFIG_HOME" => Some(OsString::from("/home/tester/.config-custom")),
            _ => None,
        })
        .expect("paths");

        assert_eq!(
            paths.config_dir(),
            Path::new("/home/tester/.config-custom/thais")
        );
        assert_eq!(
            paths.data_dir(),
            Path::new("/home/tester/.local/share/thais")
        );
        assert_eq!(
            paths.state_dir(),
            Path::new("/home/tester/.local/state/thais")
        );
        assert_eq!(paths.cache_dir(), Path::new("/home/tester/.cache/thais"));
    }

    #[cfg(unix)]
    #[test]
    fn ensure_creates_private_directories() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().expect("temp dir");
        let paths = Paths::from_roots(
            root.path().join("config"),
            root.path().join("data"),
            root.path().join("state"),
            root.path().join("cache"),
        );
        paths.ensure().expect("ensure");

        for dir in [
            paths.config_dir().to_path_buf(),
            paths.data_dir().to_path_buf(),
            paths.state_dir().to_path_buf(),
            paths.cache_dir().to_path_buf(),
            paths.sessions_dir(),
            paths.snapshots_dir(),
        ] {
            let metadata = std::fs::metadata(&dir).expect("metadata");
            assert!(metadata.is_dir());
            assert_eq!(metadata.permissions().mode() & 0o777, 0o700);
        }
    }

    #[cfg(unix)]
    #[test]
    fn write_private_creates_a_0600_file() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().expect("temp dir");
        let path = root.path().join("nested/credentials.toml");
        write_private(&path, b"secret = \"value\"").expect("write");

        assert_eq!(std::fs::read(&path).expect("read"), b"secret = \"value\"");
        let mode = std::fs::metadata(&path)
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
    }
}

#[cfg(all(test, unix))]
mod symlink_tests {
    use super::*;

    #[test]
    fn write_private_refuses_a_symlink_destination() {
        let root = tempfile::tempdir().expect("temp");
        let target = root.path().join("target");
        std::fs::write(&target, "safe").expect("target");
        let link = root.path().join("credentials.toml");
        std::os::unix::fs::symlink(&target, &link).expect("symlink");
        assert!(write_private(&link, b"secret").is_err());
        assert_eq!(std::fs::read_to_string(target).expect("target"), "safe");
    }
}
