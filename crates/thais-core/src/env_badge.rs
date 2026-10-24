//! Compact environment badge included in the system prompt.

use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::Local;
use sysinfo::System;

/// Rendered environment facts. The badge is data, not instructions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvironmentBadge {
    text: String,
}

impl EnvironmentBadge {
    /// Collect a compact, allowlisted badge for a workspace.
    pub async fn collect(workspace: &Path) -> Self {
        let mut system = System::new();
        system.refresh_memory();

        let os_name = System::name().unwrap_or_else(|| std::env::consts::OS.to_owned());
        let os_version = System::os_version().unwrap_or_default();
        let kernel = System::kernel_version().unwrap_or_default();
        let os = match (os_version.is_empty(), kernel.is_empty()) {
            (true, true) => os_name,
            (false, true) => format!("{os_name} {os_version}"),
            (true, false) => format!("{os_name}, kernel {kernel}"),
            (false, false) => format!("{os_name} {os_version}, kernel {kernel}"),
        };

        let shell = std::env::var("SHELL")
            .or_else(|_| std::env::var("COMSPEC"))
            .unwrap_or_else(|_| "unknown".to_owned());
        let cwd = display_workspace(workspace);
        let git = git_badge(workspace).await;
        let cpus = std::thread::available_parallelism()
            .map(std::num::NonZeroUsize::get)
            .unwrap_or(0);
        let ram_gb = system.total_memory() as f64 / 1024.0 / 1024.0 / 1024.0;

        let tools = ["git", "rg", "cargo", "node", "gcc", "python3", "make"]
            .into_iter()
            .filter(|tool| which::which(tool).is_ok())
            .collect::<Vec<_>>()
            .join(", ");
        let tools = if tools.is_empty() {
            "none".to_owned()
        } else {
            tools
        };

        let project = [
            "Cargo.toml",
            "package.json",
            "Makefile",
            "pyproject.toml",
            "go.mod",
        ]
        .into_iter()
        .filter(|marker| workspace.join(marker).is_file())
        .collect::<Vec<_>>()
        .join(", ");
        let project = if project.is_empty() {
            "none".to_owned()
        } else {
            project
        };

        let date = Local::now().format("%Y-%m-%d, %A");

        let text = format!(
            "<env>\nos: {os}\nshell: {shell}\ncwd: {cwd}\ngit: {git}\ncpu: {cpus} threads, ram: {ram_gb:.1} GB\ntools: {tools}\nproject: {project}\ndate: {date}\n</env>"
        );
        Self { text }
    }

    /// Badge text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.text
    }
}

async fn git_badge(workspace: &Path) -> String {
    let Some(inside) = git(workspace, &["rev-parse", "--is-inside-work-tree"]).await else {
        return "repository no".to_owned();
    };
    if inside.trim() != "true" {
        return "repository no".to_owned();
    }
    let branch = git(workspace, &["rev-parse", "--abbrev-ref", "HEAD"])
        .await
        .unwrap_or_else(|| "unknown".to_owned());
    let dirty = git(workspace, &["status", "--porcelain"])
        .await
        .is_some_and(|status| !status.trim().is_empty());
    format!(
        "repository yes, branch {branch}, dirty {}",
        if dirty { "yes" } else { "no" }
    )
}

async fn git(workspace: &Path, args: &[&str]) -> Option<String> {
    let mut command = tokio::process::Command::new("git");
    command
        .arg("-C")
        .arg(workspace)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .kill_on_drop(true);

    let output = tokio::time::timeout(Duration::from_secs(2), command.output())
        .await
        .ok()?
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn display_workspace(workspace: &Path) -> String {
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from);
    match home.as_deref() {
        Some(home) => relativize(workspace, home),
        None => workspace.display().to_string(),
    }
}

fn relativize(path: &Path, home: &Path) -> String {
    match path.strip_prefix(home) {
        Ok(relative) if relative.as_os_str().is_empty() => "~".to_owned(),
        Ok(relative) => format!("~/{}", relative.display()),
        Err(_) => path.display().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relativizes_paths_without_exposing_the_home_name() {
        let home = Path::new("/home/alice");
        assert_eq!(
            relativize(Path::new("/home/alice/projects/thais"), home),
            "~/projects/thais"
        );
        assert_eq!(relativize(Path::new("/tmp/other"), home), "/tmp/other");
    }

    #[tokio::test]
    async fn badge_contains_os_workspace_and_project_markers() {
        let temp = tempfile::tempdir().expect("temp");
        std::fs::write(temp.path().join("Cargo.toml"), "[package]\n").expect("marker");
        let badge = EnvironmentBadge::collect(temp.path()).await;
        let text = badge.as_str();

        assert!(text.contains("os:"));
        assert!(text.contains("cwd:"));
        assert!(text.contains("cpu:"));
        assert!(text.contains("project: Cargo.toml"));
        assert!(text.contains("date:"));
        assert!(!text.contains("PATH="));
        assert!(!text.contains("HOME="));
        assert!(!text.contains("API_KEY"));
    }

    #[tokio::test]
    async fn badge_reports_git_state() {
        let temp = tempfile::tempdir().expect("temp");
        let status = std::process::Command::new("git")
            .args(["init", "-q", "-b", "main"])
            .current_dir(temp.path())
            .status()
            .expect("git init");
        assert!(status.success());
        let status = std::process::Command::new("git")
            .args([
                "-c",
                "user.name=test",
                "-c",
                "user.email=test@example.invalid",
                "commit",
                "--allow-empty",
                "-qm",
                "init",
            ])
            .current_dir(temp.path())
            .status()
            .expect("git commit");
        assert!(status.success());

        let badge = EnvironmentBadge::collect(temp.path()).await;
        let text = badge.as_str();
        assert!(text.contains("repository yes"), "{text}");
        assert!(text.contains("branch main"), "{text}");
    }
}
