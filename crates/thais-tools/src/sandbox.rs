//! Best-effort OS process sandbox selection.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Sandbox selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxMode {
    /// Use an available backend, otherwise fall back to unsandboxed `ask`.
    Auto,
    /// Disable the OS sandbox.
    Off,
    /// Require a backend; refuse to run when unavailable.
    Required,
}

/// Detected sandbox backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SandboxBackend {
    /// Linux bubblewrap.
    Bubblewrap(PathBuf),
    /// macOS sandbox-exec.
    SandboxExec(PathBuf),
    /// Linux Landlock helper.
    Landlock(PathBuf),
    /// No backend is available.
    None,
}

/// Sandbox policy for shell commands.
#[derive(Debug, Clone)]
pub struct Sandbox {
    mode: SandboxMode,
    backend: SandboxBackend,
}

impl Sandbox {
    /// Detect the best backend for the current platform.
    #[must_use]
    pub fn detect(mode: SandboxMode) -> Self {
        let backend = if mode == SandboxMode::Off {
            SandboxBackend::None
        } else {
            detect_backend()
        };
        Self { mode, backend }
    }

    /// Build from an environment value: `auto`, `off`, or `required`.
    #[must_use]
    pub fn from_env() -> Self {
        let mode = match std::env::var("THAIS_SANDBOX")
            .unwrap_or_else(|_| "auto".to_owned())
            .to_ascii_lowercase()
            .as_str()
        {
            "off" | "disabled" | "0" => SandboxMode::Off,
            "required" | "on" | "1" => SandboxMode::Required,
            _ => SandboxMode::Auto,
        };
        Self::detect(mode)
    }

    /// Current backend.
    #[must_use]
    pub fn backend(&self) -> &SandboxBackend {
        &self.backend
    }

    /// Whether commands will actually be sandboxed.
    #[must_use]
    pub fn is_active(&self) -> bool {
        !matches!(self.backend, SandboxBackend::None)
    }

    /// Whether `required` mode must refuse execution.
    #[must_use]
    pub fn is_required_and_unavailable(&self) -> bool {
        self.mode == SandboxMode::Required && !self.is_active()
    }

    /// Build the command used to run a shell string.
    pub fn command(
        &self,
        shell_command: &str,
        workspace: &Path,
        cwd: &Path,
    ) -> Result<Command, String> {
        if self.is_required_and_unavailable() {
            return Err("sandbox required but no backend is available".to_owned());
        }
        match &self.backend {
            SandboxBackend::Bubblewrap(path) => {
                let mut command = Command::new(path);
                command
                    .arg("--die-with-parent")
                    .arg("--unshare-all")
                    .arg("--ro-bind")
                    .arg("/")
                    .arg("/")
                    .arg("--bind")
                    .arg(workspace)
                    .arg(workspace)
                    .arg("--dev")
                    .arg("/dev")
                    .arg("--proc")
                    .arg("/proc")
                    .arg("--chdir")
                    .arg(cwd)
                    .arg("/bin/sh")
                    .arg("-c")
                    .arg(shell_command);
                Ok(command)
            }
            SandboxBackend::Landlock(path) => {
                let mut command = Command::new(path);
                command
                    .arg("--workspace")
                    .arg(workspace)
                    .arg("--cwd")
                    .arg(cwd)
                    .arg("--")
                    .arg(shell_command);
                Ok(command)
            }
            SandboxBackend::SandboxExec(path) => {
                let profile = format!(
                    "(version 1)\n(allow default)\n(deny network*)\n(allow file-read*)\n(allow file-write* (subpath \"{}\"))\n",
                    workspace.display()
                );
                let mut command = Command::new(path);
                command
                    .arg("-p")
                    .arg(profile)
                    .arg("/bin/sh")
                    .arg("-c")
                    .arg(shell_command);
                Ok(command)
            }
            SandboxBackend::None => Ok(shell_command_command(shell_command)),
        }
    }
}

fn shell_command_command(command: &str) -> Command {
    #[cfg(windows)]
    {
        let mut process = Command::new("cmd");
        process.arg("/C").arg(command);
        process
    }
    #[cfg(not(windows))]
    {
        let mut process = Command::new("/bin/sh");
        process.arg("-c").arg(command);
        process
    }
}

fn detect_backend() -> SandboxBackend {
    #[cfg(target_os = "linux")]
    {
        if let Some(path) = which_path("bwrap") {
            return SandboxBackend::Bubblewrap(path);
        }
        if let Some(path) = helper_path() {
            return SandboxBackend::Landlock(path);
        }
    }
    #[cfg(target_os = "macos")]
    {
        if let Some(path) = which_path("sandbox-exec") {
            return SandboxBackend::SandboxExec(path);
        }
    }
    SandboxBackend::None
}

fn helper_path() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("THAIS_SANDBOX_HELPER") {
        let path = PathBuf::from(path);
        if path.is_file() {
            return Some(path);
        }
    }
    if let Ok(current) = std::env::current_exe()
        && let Some(parent) = current.parent()
    {
        let sibling = parent.join("thais-sandbox");
        if sibling.is_file() {
            return Some(sibling);
        }
    }
    which_path("thais-sandbox")
}

fn which_path(name: &str) -> Option<PathBuf> {
    let paths = std::env::var_os("PATH")?;
    for directory in std::env::split_paths(&paths) {
        let candidate = directory.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn off_is_never_active() {
        let sandbox = Sandbox::detect(SandboxMode::Off);
        assert!(!sandbox.is_active());
        assert!(!sandbox.is_required_and_unavailable());
    }

    #[test]
    fn required_without_backend_refuses() {
        let mut sandbox = Sandbox::detect(SandboxMode::Required);
        sandbox.backend = SandboxBackend::None;
        assert!(sandbox.is_required_and_unavailable());
        assert!(
            sandbox
                .command("true", Path::new("."), Path::new("."))
                .is_err()
        );
    }

    #[test]
    fn auto_without_backend_returns_a_command() {
        let mut sandbox = Sandbox::detect(SandboxMode::Auto);
        sandbox.backend = SandboxBackend::None;
        assert!(
            sandbox
                .command("true", Path::new("."), Path::new("."))
                .is_ok()
        );
    }
}
