//! Read and mutate tools exposed to the model.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use globset::Glob;
use ignore::WalkBuilder;
use regex::Regex;
use serde_json::{Value, json};

use crate::bash::BashTool;
use crate::error::ToolError;
use crate::git::{GitDiffTool, GitStatusTool};
use crate::memory::MemoryWriteTool;
use crate::mutating::{ApplyPatchTool, EditTool, WriteTool};
use crate::path_jail::PathJail;

const MAX_READ_BYTES: u64 = 2_000_000;
const MAX_OUTPUT_CHARS: usize = 200_000;
const MAX_LIST_ENTRIES: usize = 500;
const MAX_GLOB_MATCHES: usize = 500;
const MAX_GREP_MATCHES: usize = 200;

/// Tool definition exposed to the model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolDefinition {
    /// Tool name.
    pub name: String,
    /// Short description.
    pub description: String,
    /// JSON Schema for arguments.
    pub parameters: Value,
}

/// Tool output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolResult {
    /// Text returned to the model.
    pub content: String,
    /// Whether the result represents an error.
    pub is_error: bool,
    /// Optional unified diff for a mutation.
    pub diff: Option<String>,
}

impl ToolResult {
    pub(crate) fn ok(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: false,
            diff: None,
        }
    }

    pub(crate) fn ok_with_diff(content: impl Into<String>, diff: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: false,
            diff: Some(diff.into()),
        }
    }
}

/// A tool exposed to the model. Tools do synchronous I/O and are executed on a
/// blocking pool by the runtime.
pub trait Tool: Send + Sync {
    /// Tool definition.
    fn definition(&self) -> ToolDefinition;

    /// Execute the tool.
    fn execute(&self, args: Value, jail: &PathJail) -> Result<ToolResult, ToolError>;

    /// Whether the tool may change the workspace.
    fn mutating(&self) -> bool {
        false
    }

    /// Short action summary used by approval prompts.
    fn summary(&self, args: &Value) -> String {
        let name = self.definition().name;
        let path = args.get("path").and_then(Value::as_str).unwrap_or("");
        if path.is_empty() {
            name
        } else {
            format!("{name} {path}")
        }
    }

    /// Preview the effect of a mutation without applying it.
    fn preview(&self, _args: &Value, _jail: &PathJail) -> Option<String> {
        None
    }
}

/// Registry of M1 read-only tools and M2 mutation tools.
pub struct ToolRegistry {
    jail: PathJail,
    tools: HashMap<String, Arc<dyn Tool>>,
}

impl ToolRegistry {
    /// Build the read-only registry for a workspace.
    pub fn read_only(root: &Path) -> Result<Self, ToolError> {
        let jail = PathJail::new(root)?;
        let mut tools: HashMap<String, Arc<dyn Tool>> = HashMap::new();
        tools.insert("read".to_owned(), Arc::new(ReadTool));
        tools.insert("list".to_owned(), Arc::new(ListTool));
        tools.insert("glob".to_owned(), Arc::new(GlobTool));
        tools.insert("grep".to_owned(), Arc::new(GrepTool));
        tools.insert("git_status".to_owned(), Arc::new(GitStatusTool));
        tools.insert("git_diff".to_owned(), Arc::new(GitDiffTool));
        Ok(Self { jail, tools })
    }

    /// Build the full M2 registry for a workspace.
    pub fn read_write(root: &Path) -> Result<Self, ToolError> {
        let mut registry = Self::read_only(root)?;
        registry.tools.insert("edit".to_owned(), Arc::new(EditTool));
        registry
            .tools
            .insert("write".to_owned(), Arc::new(WriteTool));
        registry
            .tools
            .insert("apply_patch".to_owned(), Arc::new(ApplyPatchTool));
        registry.tools.insert("bash".to_owned(), Arc::new(BashTool));
        registry
            .tools
            .insert("memory_write".to_owned(), Arc::new(MemoryWriteTool));
        Ok(registry)
    }

    /// Definitions in stable name order.
    #[must_use]
    pub fn definitions(&self) -> Vec<ToolDefinition> {
        let mut definitions = self
            .tools
            .values()
            .map(|tool| tool.definition())
            .collect::<Vec<_>>();
        definitions.sort_by(|left, right| left.name.cmp(&right.name));
        definitions
    }

    /// Execute a tool by name.
    pub fn execute(&self, name: &str, args: Value) -> Result<ToolResult, ToolError> {
        let tool = self
            .tools
            .get(name)
            .ok_or_else(|| ToolError::UnknownTool(name.to_owned()))?;
        tool.execute(args, &self.jail)
    }

    /// Whether a tool may mutate the workspace.
    #[must_use]
    pub fn is_mutating(&self, name: &str) -> bool {
        self.tools.get(name).is_some_and(|tool| tool.mutating())
    }

    /// Human-readable action summary for approvals.
    #[must_use]
    pub fn summary(&self, name: &str, args: &Value) -> String {
        self.tools
            .get(name)
            .map_or_else(|| name.to_owned(), |tool| tool.summary(args))
    }

    /// Preview a mutation without applying it.
    #[must_use]
    pub fn preview(&self, name: &str, args: &Value) -> Option<String> {
        self.tools
            .get(name)
            .and_then(|tool| tool.preview(args, &self.jail))
    }

    /// Workspace jail.
    #[must_use]
    pub fn jail(&self) -> &PathJail {
        &self.jail
    }
}

struct ReadTool;

impl Tool for ReadTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "read".to_owned(),
            description:
                "Read a text file with line numbers. Use offset and limit for large files."
                    .to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "offset": {"type": "integer", "minimum": 1},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 5000}
                },
                "required": ["path"],
                "additionalProperties": false
            }),
        }
    }

    fn execute(&self, args: Value, jail: &PathJail) -> Result<ToolResult, ToolError> {
        let path = required_str(&args, "path")?;
        let path = jail.resolve(path)?;
        let metadata = metadata(&path)?;
        if metadata.is_dir() {
            return Err(ToolError::InvalidArgs(format!(
                "{} is a directory; use list or glob",
                jail.display(&path)
            )));
        }
        if metadata.len() > MAX_READ_BYTES {
            return Err(ToolError::InvalidArgs(format!(
                "{} is too large ({} bytes); use a smaller file",
                jail.display(&path),
                metadata.len()
            )));
        }

        let bytes = std::fs::read(&path).map_err(|source| ToolError::Io {
            path: path.clone(),
            source,
        })?;
        if bytes.iter().take(8192).any(|byte| *byte == 0) {
            return Err(ToolError::Binary(path));
        }
        let text = String::from_utf8_lossy(&bytes);
        let offset = args
            .get("offset")
            .and_then(Value::as_u64)
            .unwrap_or(1)
            .max(1) as usize;
        let limit = args
            .get("limit")
            .and_then(Value::as_u64)
            .unwrap_or(2000)
            .clamp(1, 5000) as usize;

        let mut output = String::new();
        for (index, line) in text.lines().enumerate().skip(offset - 1).take(limit) {
            output.push_str(&format!("{:>6}\t{}\n", index + 1, line));
        }
        if output.is_empty() {
            output.push_str("(no lines)\n");
        }
        Ok(ToolResult::ok(cap_output(output)))
    }
}

struct ListTool;

impl Tool for ListTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "list".to_owned(),
            description: "List a directory. Paths are relative to the workspace.".to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"}
                },
                "additionalProperties": false
            }),
        }
    }

    fn execute(&self, args: Value, jail: &PathJail) -> Result<ToolResult, ToolError> {
        let requested = args.get("path").and_then(Value::as_str).unwrap_or(".");
        let path = jail.resolve(requested)?;
        let metadata = metadata(&path)?;
        if !metadata.is_dir() {
            return Err(ToolError::InvalidArgs(format!(
                "{} is not a directory",
                jail.display(&path)
            )));
        }

        let mut entries = std::fs::read_dir(&path)
            .map_err(|source| ToolError::Io {
                path: path.clone(),
                source,
            })?
            .filter_map(Result::ok)
            .collect::<Vec<_>>();
        entries.sort_by_key(std::fs::DirEntry::file_name);

        let mut output = String::new();
        for entry in entries.iter().take(MAX_LIST_ENTRIES) {
            let name = entry.file_name().to_string_lossy().into_owned();
            let metadata = entry.metadata().ok();
            let kind = if metadata.as_ref().is_some_and(std::fs::Metadata::is_dir) {
                format!("[dir]  {name}")
            } else if metadata.as_ref().is_some_and(std::fs::Metadata::is_symlink) {
                format!("[link] {name}")
            } else {
                let size = metadata.as_ref().map_or(0, std::fs::Metadata::len);
                format!("[file] {name} ({size} bytes)")
            };
            output.push_str(&kind);
            output.push('\n');
        }
        if entries.len() > MAX_LIST_ENTRIES {
            output.push_str("[truncated]\n");
        }
        if output.is_empty() {
            output.push_str("(empty directory)\n");
        }
        Ok(ToolResult::ok(cap_output(output)))
    }
}

struct GlobTool;

impl Tool for GlobTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "glob".to_owned(),
            description: "Find files by glob pattern, for example **/*.rs.".to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "pattern": {"type": "string"},
                    "path": {"type": "string"}
                },
                "required": ["pattern"],
                "additionalProperties": false
            }),
        }
    }

    fn execute(&self, args: Value, jail: &PathJail) -> Result<ToolResult, ToolError> {
        let pattern = required_str(&args, "pattern")?;
        let matcher = Glob::new(pattern)
            .map_err(|error| ToolError::InvalidArgs(format!("invalid glob: {error}")))?
            .compile_matcher();
        let requested = args.get("path").and_then(Value::as_str).unwrap_or(".");
        let base = jail.resolve(requested)?;
        if !metadata(&base)?.is_dir() {
            return Err(ToolError::InvalidArgs(format!(
                "{} is not a directory",
                jail.display(&base)
            )));
        }

        let mut matches = Vec::new();
        for entry in walk(&base) {
            let Ok(entry) = entry else { continue };
            if !entry.file_type().is_some_and(|kind| kind.is_file()) {
                continue;
            }
            let relative = entry
                .path()
                .strip_prefix(&base)
                .unwrap_or_else(|_| entry.path());
            if matcher.is_match(relative) {
                matches.push(jail.display(entry.path()));
                if matches.len() >= MAX_GLOB_MATCHES {
                    break;
                }
            }
        }
        if matches.is_empty() {
            return Ok(ToolResult::ok("(no matches)"));
        }
        if matches.len() >= MAX_GLOB_MATCHES {
            matches.push("[truncated]".to_owned());
        }
        Ok(ToolResult::ok(matches.join("\n")))
    }
}

struct GrepTool;

impl Tool for GrepTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "grep".to_owned(),
            description: "Search text files with a regular expression.".to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "pattern": {"type": "string"},
                    "path": {"type": "string"},
                    "glob": {"type": "string"}
                },
                "required": ["pattern"],
                "additionalProperties": false
            }),
        }
    }

    fn execute(&self, args: Value, jail: &PathJail) -> Result<ToolResult, ToolError> {
        let pattern = required_str(&args, "pattern")?;
        let regex = Regex::new(pattern)
            .map_err(|error| ToolError::InvalidArgs(format!("invalid regex: {error}")))?;
        let requested = args.get("path").and_then(Value::as_str).unwrap_or(".");
        let base = jail.resolve(requested)?;
        let glob = args
            .get("glob")
            .and_then(Value::as_str)
            .map(Glob::new)
            .transpose()
            .map_err(|error| ToolError::InvalidArgs(format!("invalid glob: {error}")))?
            .map(|glob| glob.compile_matcher());

        let files = if metadata(&base)?.is_file() {
            vec![base.clone()]
        } else {
            walk(&base)
                .filter_map(Result::ok)
                .filter(|entry| entry.file_type().is_some_and(|kind| kind.is_file()))
                .map(|entry| entry.path().to_path_buf())
                .collect::<Vec<_>>()
        };

        let mut matches = Vec::new();
        for path in files {
            if let Some(glob) = &glob {
                let relative = path.strip_prefix(&base).unwrap_or(&path);
                if !glob.is_match(relative) {
                    continue;
                }
            }
            let Ok(metadata) = std::fs::metadata(&path) else {
                continue;
            };
            if metadata.len() > MAX_READ_BYTES {
                continue;
            }
            let Ok(bytes) = std::fs::read(&path) else {
                continue;
            };
            if bytes.iter().take(8192).any(|byte| *byte == 0) {
                continue;
            }
            let text = String::from_utf8_lossy(&bytes);
            for (index, line) in text.lines().enumerate() {
                if regex.is_match(line) {
                    matches.push(format!("{}:{}: {}", jail.display(&path), index + 1, line));
                    if matches.len() >= MAX_GREP_MATCHES {
                        break;
                    }
                }
            }
            if matches.len() >= MAX_GREP_MATCHES {
                break;
            }
        }

        if matches.is_empty() {
            return Ok(ToolResult::ok("(no matches)"));
        }
        if matches.len() >= MAX_GREP_MATCHES {
            matches.push("[truncated]".to_owned());
        }
        Ok(ToolResult::ok(cap_output(matches.join("\n"))))
    }
}

fn walk(base: &Path) -> impl Iterator<Item = Result<ignore::DirEntry, ignore::Error>> {
    WalkBuilder::new(base)
        .hidden(true)
        .git_ignore(true)
        .filter_entry(|entry| {
            let name = entry.file_name().to_string_lossy();
            !matches!(name.as_ref(), ".git" | "target" | "node_modules" | ".thais")
        })
        .build()
}

pub(crate) fn metadata(path: &Path) -> Result<std::fs::Metadata, ToolError> {
    std::fs::metadata(path).map_err(|source| {
        if source.kind() == std::io::ErrorKind::NotFound {
            ToolError::NotFound(path.to_path_buf())
        } else {
            ToolError::Io {
                path: path.to_path_buf(),
                source,
            }
        }
    })
}

pub(crate) fn required_str<'a>(args: &'a Value, key: &str) -> Result<&'a str, ToolError> {
    args.get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| ToolError::InvalidArgs(format!("missing `{key}`")))
}

pub(crate) fn cap_output(mut output: String) -> String {
    if output.chars().count() > MAX_OUTPUT_CHARS {
        output = output.chars().take(MAX_OUTPUT_CHARS).collect();
        output.push_str("\n[truncated]");
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace() -> (tempfile::TempDir, ToolRegistry) {
        let temp = tempfile::tempdir().expect("temp");
        std::fs::create_dir_all(temp.path().join("src")).expect("src");
        std::fs::write(temp.path().join("src/lib.rs"), "pub fn alpha() {}\n").expect("lib");
        std::fs::write(temp.path().join("README.md"), "# demo\nalpha beta\n").expect("readme");
        let registry = ToolRegistry::read_only(temp.path()).expect("registry");
        (temp, registry)
    }

    #[test]
    fn read_returns_line_numbers() {
        let (_temp, registry) = workspace();
        let result = registry
            .execute("read", json!({"path": "src/lib.rs"}))
            .expect("read");
        assert!(!result.is_error);
        assert!(result.content.contains("1\tpub fn alpha"));
    }

    #[test]
    fn list_and_glob_find_workspace_files() {
        let (_temp, registry) = workspace();
        let list = registry
            .execute("list", json!({"path": "."}))
            .expect("list");
        assert!(list.content.contains("[dir]  src"));
        assert!(list.content.contains("README.md"));

        let glob = registry
            .execute("glob", json!({"pattern": "**/*.rs"}))
            .expect("glob");
        assert!(glob.content.contains("src/lib.rs"));
    }

    #[test]
    fn grep_returns_file_and_line() {
        let (_temp, registry) = workspace();
        let result = registry
            .execute("grep", json!({"pattern": "alpha"}))
            .expect("grep");
        assert!(result.content.contains("src/lib.rs:1"));
        assert!(result.content.contains("README.md:2"));
    }

    #[test]
    fn path_escape_is_rejected() {
        let (_temp, registry) = workspace();
        let result = registry.execute("read", json!({"path": "../outside"}));
        assert!(matches!(result, Err(ToolError::Escape(_))));
    }

    #[test]
    fn binary_files_are_rejected() {
        let temp = tempfile::tempdir().expect("temp");
        std::fs::write(temp.path().join("blob.bin"), [0, 1, 2, 3]).expect("blob");
        let registry = ToolRegistry::read_only(temp.path()).expect("registry");
        let result = registry.execute("read", json!({"path": "blob.bin"}));
        assert!(matches!(result, Err(ToolError::Binary(_))));
    }

    #[test]
    fn definitions_are_stable() {
        let (_temp, registry) = workspace();
        let names: Vec<_> = registry
            .definitions()
            .into_iter()
            .map(|definition| definition.name)
            .collect();
        assert_eq!(
            names,
            vec!["git_diff", "git_status", "glob", "grep", "list", "read"]
        );
    }
}
