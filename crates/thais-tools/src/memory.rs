//! Approved project memory (`MEMORY.md`).

use std::path::Path;

use serde_json::{Value, json};

use crate::error::ToolError;
use crate::path_jail::PathJail;
use crate::tools::{Tool, ToolDefinition, ToolResult, cap_output};

/// Project memory file name.
pub const MEMORY_FILE: &str = "MEMORY.md";
/// Hard cap for the complete memory file.
pub const MAX_MEMORY_TOKENS: usize = 2_000;
/// Hard cap for reading the memory file in one call.
pub const MAX_MEMORY_BYTES: u64 = 32_768;
/// Header used when the file is first created.
pub const MEMORY_HEADER: &str =
    "# Project memory\n\n<!-- Thais-managed approved rules. Lower priority than AGENTS.md. -->\n\n";

/// Read the approved project memory file.
pub struct MemoryReadTool;

impl Tool for MemoryReadTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "memory_read".to_owned(),
            description:
                "Read MEMORY.md with line numbers. Use offset and limit for large memory files."
                    .to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "offset": {"type": "integer", "minimum": 1},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 2000}
                },
                "additionalProperties": false
            }),
        }
    }

    fn execute(&self, args: Value, jail: &PathJail) -> Result<ToolResult, ToolError> {
        let path = jail.resolve(MEMORY_FILE)?;
        let text = read_memory(&path)?;
        let offset = args
            .get("offset")
            .and_then(Value::as_u64)
            .unwrap_or(1)
            .max(1) as usize;
        let limit = args
            .get("limit")
            .and_then(Value::as_u64)
            .unwrap_or(2000)
            .clamp(1, 2000) as usize;
        let mut output = String::new();
        for (index, line) in text.lines().enumerate().skip(offset - 1).take(limit) {
            output.push_str(&format!("{:>6}\t{}\n", index + 1, line));
        }
        if output.is_empty() {
            output.push_str("(MEMORY.md is empty)\n");
        }
        Ok(ToolResult::ok(cap_output(output)))
    }
}

/// Search MEMORY.md for a literal, case-insensitive query.
pub struct MemorySearchTool;

impl Tool for MemorySearchTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "memory_search".to_owned(),
            description: "Search MEMORY.md and return matching lines with line numbers.".to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string", "minLength": 1},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 50}
                },
                "required": ["query"],
                "additionalProperties": false
            }),
        }
    }

    fn execute(&self, args: Value, jail: &PathJail) -> Result<ToolResult, ToolError> {
        let query = args
            .get("query")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|query| !query.is_empty())
            .ok_or_else(|| ToolError::InvalidArgs("missing `query`".to_owned()))?;
        if query.chars().count() > 256 {
            return Err(ToolError::InvalidArgs(
                "query is too long; keep it under 256 characters".to_owned(),
            ));
        }
        let limit = args
            .get("limit")
            .and_then(Value::as_u64)
            .unwrap_or(20)
            .clamp(1, 50) as usize;
        let path = jail.resolve(MEMORY_FILE)?;
        let text = read_memory(&path)?;
        let needle = query.to_lowercase();
        let mut matches = Vec::new();
        for (index, line) in text.lines().enumerate() {
            if line.to_lowercase().contains(&needle) {
                matches.push(format!("MEMORY.md:{}: {}", index + 1, line.trim()));
                if matches.len() >= limit {
                    break;
                }
            }
        }
        if matches.is_empty() {
            return Ok(ToolResult::ok("(no memory matches)"));
        }
        if matches.len() >= limit {
            matches.push("[truncated]".to_owned());
        }
        Ok(ToolResult::ok(cap_output(matches.join("\n"))))
    }
}

/// Tool that appends one approved rule to `MEMORY.md`.
pub struct MemoryWriteTool;

impl Tool for MemoryWriteTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "memory_write".to_owned(),
            description:
                "Append one durable project rule to MEMORY.md. Requires approval. Use only for decisions or project facts that should survive sessions; never for secrets."
                    .to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "rule": {"type": "string", "minLength": 1},
                    "reason": {"type": "string"}
                },
                "required": ["rule"],
                "additionalProperties": false
            }),
        }
    }

    fn execute(&self, args: Value, jail: &PathJail) -> Result<ToolResult, ToolError> {
        let rule = required_rule(&args)?;
        let path = jail.resolve(MEMORY_FILE)?;
        let current = read_memory(&path)?;
        let proposed = append_rule(&current, &rule)?;
        atomic_write(&path, proposed.as_bytes())?;
        Ok(ToolResult::ok_with_diff(
            format!("remembered: {rule}"),
            memory_diff(&current, &proposed),
        ))
    }

    fn mutating(&self) -> bool {
        true
    }

    fn summary(&self, args: &Value) -> String {
        let rule = args.get("rule").and_then(Value::as_str).unwrap_or("");
        format!("memory_write {}", truncate(rule, 80))
    }

    fn preview(&self, args: &Value, jail: &PathJail) -> Option<String> {
        let rule = required_rule(args).ok()?;
        let path = jail.resolve(MEMORY_FILE).ok()?;
        let current = read_memory(&path).ok()?;
        let proposed = append_rule(&current, &rule).ok()?;
        Some(memory_diff(&current, &proposed))
    }
}

fn required_rule(args: &Value) -> Result<String, ToolError> {
    let rule = args
        .get("rule")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or_default();
    if rule.is_empty() {
        return Err(ToolError::InvalidArgs("rule must not be empty".to_owned()));
    }
    if rule.chars().count() > 500 {
        return Err(ToolError::InvalidArgs(
            "rule is too long; keep it under 500 characters".to_owned(),
        ));
    }
    if rule.contains('\n') {
        return Err(ToolError::InvalidArgs(
            "rule must be a single line".to_owned(),
        ));
    }
    if looks_like_secret(rule) {
        return Err(ToolError::InvalidArgs(
            "refusing to store a value that looks like a secret".to_owned(),
        ));
    }
    Ok(rule.to_owned())
}

fn looks_like_secret(rule: &str) -> bool {
    let lower = rule.to_ascii_lowercase();
    lower.contains("sk-")
        || lower.contains("ghp_")
        || lower.contains("xoxb-")
        || lower.contains("api_key=")
        || lower.contains("password=")
        || lower.contains("secret=")
        || lower.contains("-----begin")
}

fn read_memory(path: &Path) -> Result<String, ToolError> {
    if let Ok(metadata) = std::fs::metadata(path)
        && metadata.len() > MAX_MEMORY_BYTES
    {
        return Err(ToolError::InvalidArgs(format!(
            "MEMORY.md is too large ({} bytes); maximum is {MAX_MEMORY_BYTES}",
            metadata.len()
        )));
    }
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(text),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(source) => Err(ToolError::Io {
            path: path.to_path_buf(),
            source,
        }),
    }
}

fn append_rule(current: &str, rule: &str) -> Result<String, ToolError> {
    let normalized = normalize(rule);
    if existing_rules(current)
        .iter()
        .any(|existing| normalize(existing) == normalized)
    {
        return Err(ToolError::InvalidArgs(
            "an equivalent memory rule already exists".to_owned(),
        ));
    }

    let mut proposed = current.to_owned();
    if proposed.trim().is_empty() {
        proposed.push_str(MEMORY_HEADER);
    } else {
        if !proposed.ends_with('\n') {
            proposed.push('\n');
        }
        if !proposed.ends_with("\n\n") {
            proposed.push('\n');
        }
    }
    proposed.push_str("- ");
    proposed.push_str(rule);
    proposed.push('\n');

    let estimated = proposed.chars().count() / 4 + 1;
    if estimated > MAX_MEMORY_TOKENS {
        return Err(ToolError::InvalidArgs(format!(
            "MEMORY.md would exceed the {MAX_MEMORY_TOKENS}-token cap"
        )));
    }
    Ok(proposed)
}

fn existing_rules(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim)
        .filter_map(|line| line.strip_prefix("- "))
        .map(ToOwned::to_owned)
        .collect()
}

fn normalize(value: &str) -> String {
    value
        .chars()
        .filter(|character| character.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

fn truncate(value: &str, max: usize) -> String {
    if value.chars().count() <= max {
        value.to_owned()
    } else {
        format!("{}…", value.chars().take(max).collect::<String>())
    }
}

fn memory_diff(current: &str, proposed: &str) -> String {
    let patch = diffy::create_patch(current, proposed).to_string();
    if patch.is_empty() {
        patch
    } else {
        format!("--- a/{MEMORY_FILE}\n+++ b/{MEMORY_FILE}\n{patch}")
    }
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), ToolError> {
    let parent = path
        .parent()
        .ok_or_else(|| ToolError::InvalidArgs(format!("path has no parent: {}", path.display())))?;
    std::fs::create_dir_all(parent).map_err(|source| ToolError::Io {
        path: parent.to_path_buf(),
        source,
    })?;
    let mut temporary =
        tempfile::NamedTempFile::new_in(parent).map_err(|source| ToolError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
    std::io::Write::write_all(&mut temporary, bytes).map_err(|source| ToolError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    temporary.persist(path).map_err(|error| ToolError::Io {
        path: path.to_path_buf(),
        source: error.error,
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn jail(root: &Path) -> PathJail {
        PathJail::new(root).expect("jail")
    }

    #[test]
    fn appends_once_and_rejects_duplicates() {
        let temp = tempfile::tempdir().expect("temp");
        let tool = MemoryWriteTool;
        let result = tool
            .execute(
                json!({"rule": "Use `make check` before commits"}),
                &jail(temp.path()),
            )
            .expect("write");
        assert!(result.diff.expect("diff").contains("MEMORY.md"));
        let path = temp.path().join(MEMORY_FILE);
        let text = std::fs::read_to_string(&path).expect("memory");
        assert!(text.contains("make check"));

        let error = tool
            .execute(
                json!({"rule": "use make check before commits"}),
                &jail(temp.path()),
            )
            .expect_err("duplicate");
        assert!(error.to_string().contains("already exists"));
    }

    #[test]
    fn rejects_secrets_and_cap_overflow() {
        let temp = tempfile::tempdir().expect("temp");
        let tool = MemoryWriteTool;
        assert!(
            tool.execute(json!({"rule": "api_key=sk-live"}), &jail(temp.path()))
                .is_err()
        );
        let huge = "x".repeat(9_000);
        assert!(
            tool.execute(json!({"rule": huge}), &jail(temp.path()))
                .is_err()
        );
    }
}

#[cfg(test)]
mod read_tests {
    use super::*;

    fn jail(root: &Path) -> PathJail {
        PathJail::new(root).expect("jail")
    }

    #[test]
    fn reads_with_offsets_and_searches_case_insensitively() {
        let temp = tempfile::tempdir().expect("temp");
        std::fs::write(
            temp.path().join(MEMORY_FILE),
            "# Memory\n- Use make check\n- Keep changes small\n",
        )
        .expect("memory");
        let read = MemoryReadTool
            .execute(json!({"offset": 2, "limit": 1}), &jail(temp.path()))
            .expect("read");
        assert!(read.content.contains("2\t- Use make check"));
        assert!(!read.content.contains("Keep changes small"));

        let search = MemorySearchTool
            .execute(
                json!({"query": "MAKE CHECK", "limit": 5}),
                &jail(temp.path()),
            )
            .expect("search");
        assert!(search.content.contains("MEMORY.md:2"));
        assert!(search.content.contains("Use make check"));
    }

    #[test]
    fn huge_memory_is_rejected() {
        let temp = tempfile::tempdir().expect("temp");
        std::fs::write(
            temp.path().join(MEMORY_FILE),
            "x".repeat((MAX_MEMORY_BYTES + 1) as usize),
        )
        .expect("memory");
        assert!(
            MemoryReadTool
                .execute(json!({}), &jail(temp.path()))
                .is_err()
        );
    }

    #[test]
    fn missing_memory_is_empty_and_query_is_required() {
        let temp = tempfile::tempdir().expect("temp");
        let read = MemoryReadTool
            .execute(json!({}), &jail(temp.path()))
            .expect("read");
        assert!(read.content.contains("empty"));
        assert!(
            MemorySearchTool
                .execute(json!({"query": "  "}), &jail(temp.path()))
                .is_err()
        );
    }
}
