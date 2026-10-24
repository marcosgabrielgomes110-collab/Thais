//! Mutating text tools: write, edit, and apply_patch.

use std::io::Write;
use std::path::Path;

use serde_json::{Value, json};

use crate::error::ToolError;
use crate::path_jail::PathJail;
use crate::tools::{Tool, ToolDefinition, ToolResult, cap_output, metadata, required_str};

const MAX_MUTATION_BYTES: u64 = 2_000_000;

/// Write a text file atomically.
pub struct WriteTool;

impl Tool for WriteTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "write".to_owned(),
            description: "Create or replace a text file. The write is atomic.".to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "content": {"type": "string"}
                },
                "required": ["path", "content"],
                "additionalProperties": false
            }),
        }
    }

    fn mutating(&self) -> bool {
        true
    }

    fn summary(&self, args: &Value) -> String {
        format!(
            "write {}",
            args.get("path").and_then(Value::as_str).unwrap_or("")
        )
    }

    fn preview(&self, args: &Value, jail: &PathJail) -> Option<String> {
        let path = jail.resolve(args.get("path")?.as_str()?).ok()?;
        let content = args.get("content")?.as_str()?;
        let old = read_text_if_exists(&path, jail).ok()?;
        Some(unified_diff(&jail.display(&path), &old, content))
    }

    fn execute(&self, args: Value, jail: &PathJail) -> Result<ToolResult, ToolError> {
        let path = jail.resolve(required_str(&args, "path")?)?;
        let content = string_arg(&args, "content")?;
        if content.len() as u64 > MAX_MUTATION_BYTES {
            return Err(ToolError::InvalidArgs("content is too large".to_owned()));
        }
        if content.as_bytes().contains(&0) {
            return Err(ToolError::Binary(path));
        }
        let old = read_text_if_exists(&path, jail)?;
        atomic_write(&path, content.as_bytes())?;
        let diff = unified_diff(&jail.display(&path), &old, content);
        Ok(ToolResult::ok_with_diff(
            format!("wrote {} ({} bytes)", jail.display(&path), content.len()),
            diff,
        ))
    }
}

/// Replace text in a file.
pub struct EditTool;

impl Tool for EditTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "edit".to_owned(),
            description:
                "Replace an exact string in a text file. Use replace_all for every occurrence."
                    .to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "old_string": {"type": "string"},
                    "new_string": {"type": "string"},
                    "replace_all": {"type": "boolean"}
                },
                "required": ["path", "old_string", "new_string"],
                "additionalProperties": false
            }),
        }
    }

    fn mutating(&self) -> bool {
        true
    }

    fn summary(&self, args: &Value) -> String {
        format!(
            "edit {}",
            args.get("path").and_then(Value::as_str).unwrap_or("")
        )
    }

    fn preview(&self, args: &Value, jail: &PathJail) -> Option<String> {
        let path = jail.resolve(args.get("path")?.as_str()?).ok()?;
        let old_string = args.get("old_string")?.as_str()?;
        let new_string = args.get("new_string")?.as_str()?;
        let replace_all = args
            .get("replace_all")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let original = read_text(&path, jail).ok()?;
        if !original.contains(old_string) {
            return None;
        }
        let updated = if replace_all {
            original.replace(old_string, new_string)
        } else {
            original.replacen(old_string, new_string, 1)
        };
        Some(unified_diff(&jail.display(&path), &original, &updated))
    }

    fn execute(&self, args: Value, jail: &PathJail) -> Result<ToolResult, ToolError> {
        let path = jail.resolve(required_str(&args, "path")?)?;
        let old_string = required_str(&args, "old_string")?;
        let new_string = string_arg(&args, "new_string")?;
        let replace_all = args
            .get("replace_all")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let original = read_text(&path, jail)?;
        if !original.contains(old_string) {
            return Err(ToolError::InvalidArgs(
                "old_string was not found in the file".to_owned(),
            ));
        }
        let count = original.matches(old_string).count();
        let updated = if replace_all {
            original.replace(old_string, new_string)
        } else {
            original.replacen(old_string, new_string, 1)
        };
        if updated == original {
            return Err(ToolError::InvalidArgs(
                "the edit would not change the file".to_owned(),
            ));
        }
        atomic_write(&path, updated.as_bytes())?;
        let diff = unified_diff(&jail.display(&path), &original, &updated);
        Ok(ToolResult::ok_with_diff(
            format!(
                "edited {} ({} replacement{})",
                jail.display(&path),
                count,
                if count == 1 { "" } else { "s" }
            ),
            diff,
        ))
    }
}

/// Apply a unified diff to a file.
pub struct ApplyPatchTool;

impl Tool for ApplyPatchTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "apply_patch".to_owned(),
            description:
                "Apply a unified diff to a text file. The path is separate from the patch headers."
                    .to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "patch": {"type": "string"}
                },
                "required": ["path", "patch"],
                "additionalProperties": false
            }),
        }
    }

    fn mutating(&self) -> bool {
        true
    }

    fn summary(&self, args: &Value) -> String {
        format!(
            "apply_patch {}",
            args.get("path").and_then(Value::as_str).unwrap_or("")
        )
    }

    fn preview(&self, args: &Value, jail: &PathJail) -> Option<String> {
        let path = jail.resolve(args.get("path")?.as_str()?).ok()?;
        let patch_text = args.get("patch")?.as_str()?;
        let original = read_text(&path, jail).ok()?;
        let patch = diffy::Patch::from_str(patch_text).ok()?;
        let updated = diffy::apply(&original, &patch).ok()?;
        Some(unified_diff(&jail.display(&path), &original, &updated))
    }

    fn execute(&self, args: Value, jail: &PathJail) -> Result<ToolResult, ToolError> {
        let path = jail.resolve(required_str(&args, "path")?)?;
        let patch_text = string_arg(&args, "patch")?;
        let original = read_text(&path, jail)?;
        let patch = diffy::Patch::from_str(patch_text)
            .map_err(|error| ToolError::InvalidArgs(format!("invalid patch: {error}")))?;
        let updated = diffy::apply(&original, &patch)
            .map_err(|error| ToolError::InvalidArgs(format!("patch does not apply: {error}")))?;
        if updated == original {
            return Err(ToolError::InvalidArgs(
                "the patch would not change the file".to_owned(),
            ));
        }
        atomic_write(&path, updated.as_bytes())?;
        let diff = unified_diff(&jail.display(&path), &original, &updated);
        Ok(ToolResult::ok_with_diff(
            format!("patched {}", jail.display(&path)),
            diff,
        ))
    }
}

fn string_arg<'a>(args: &'a Value, key: &str) -> Result<&'a str, ToolError> {
    args.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| ToolError::InvalidArgs(format!("missing `{key}`")))
}

fn read_text_if_exists(path: &Path, jail: &PathJail) -> Result<String, ToolError> {
    if path.exists() {
        read_text(path, jail)
    } else {
        Ok(String::new())
    }
}

fn read_text(path: &Path, jail: &PathJail) -> Result<String, ToolError> {
    let metadata = metadata(path)?;
    if metadata.is_dir() {
        return Err(ToolError::InvalidArgs(format!(
            "{} is a directory",
            jail.display(path)
        )));
    }
    if metadata.len() > MAX_MUTATION_BYTES {
        return Err(ToolError::InvalidArgs(format!(
            "{} is too large",
            jail.display(path)
        )));
    }
    let bytes = std::fs::read(path).map_err(|source| ToolError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    if bytes.iter().take(8192).any(|byte| *byte == 0) {
        return Err(ToolError::Binary(path.to_path_buf()));
    }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

fn atomic_write(path: &Path, content: &[u8]) -> Result<(), ToolError> {
    let parent = path
        .parent()
        .ok_or_else(|| ToolError::InvalidArgs(format!("path has no parent: {}", path.display())))?;
    std::fs::create_dir_all(parent).map_err(|source| ToolError::Io {
        path: parent.to_path_buf(),
        source,
    })?;
    let mut temp = tempfile::NamedTempFile::new_in(parent).map_err(|source| ToolError::Io {
        path: parent.to_path_buf(),
        source,
    })?;
    temp.write_all(content).map_err(|source| ToolError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    temp.flush().map_err(|source| ToolError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    if let Ok(existing) = std::fs::metadata(path) {
        let _ = temp.as_file().set_permissions(existing.permissions());
    }
    match temp.persist(path) {
        Ok(_) => Ok(()),
        Err(error) => {
            #[cfg(windows)]
            {
                if path.exists() {
                    std::fs::remove_file(path).map_err(|source| ToolError::Io {
                        path: path.to_path_buf(),
                        source,
                    })?;
                    error.file.persist(path).map_err(|persist| ToolError::Io {
                        path: path.to_path_buf(),
                        source: persist.error,
                    })?;
                    return Ok(());
                }
            }
            Err(ToolError::Io {
                path: path.to_path_buf(),
                source: error.error,
            })
        }
    }
}

fn unified_diff(path: &str, old: &str, new: &str) -> String {
    cap_output(
        similar::TextDiff::from_lines(old, new)
            .unified_diff()
            .header(&format!("a/{path}"), &format!("b/{path}"))
            .to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::ToolRegistry;
    use serde_json::json;

    fn registry() -> (tempfile::TempDir, ToolRegistry) {
        let temp = tempfile::tempdir().expect("temp");
        let registry = ToolRegistry::read_write(temp.path()).expect("registry");
        (temp, registry)
    }

    #[test]
    fn write_and_edit_return_diffs() {
        let (_temp, registry) = registry();
        let result = registry
            .execute(
                "write",
                json!({"path": "src/lib.rs", "content": "let a = 1;\n"}),
            )
            .expect("write");
        assert!(result.diff.as_deref().unwrap_or("").contains("+let a = 1;"));

        let result = registry
            .execute(
                "edit",
                json!({
                    "path": "src/lib.rs",
                    "old_string": "1",
                    "new_string": "2"
                }),
            )
            .expect("edit");
        assert!(result.diff.as_deref().unwrap_or("").contains("-let a = 1;"));
        assert!(result.diff.as_deref().unwrap_or("").contains("+let a = 2;"));
    }

    #[test]
    fn apply_patch_updates_a_file() {
        let (temp, registry) = registry();
        std::fs::write(temp.path().join("file.txt"), "a\nb\n").expect("file");
        let patch = "--- a/file.txt\n+++ b/file.txt\n@@ -1,2 +1,2 @@\n a\n-b\n+c\n";
        let result = registry
            .execute("apply_patch", json!({"path": "file.txt", "patch": patch}))
            .expect("patch");
        assert!(result.diff.as_deref().unwrap_or("").contains("+c"));
        assert_eq!(
            std::fs::read_to_string(temp.path().join("file.txt")).expect("read"),
            "a\nc\n"
        );
    }

    #[test]
    fn edit_rejects_a_missing_old_string() {
        let (temp, registry) = registry();
        std::fs::write(temp.path().join("file.txt"), "a\n").expect("file");
        let result = registry.execute(
            "edit",
            json!({"path": "file.txt", "old_string": "missing", "new_string": "x"}),
        );
        assert!(matches!(result, Err(ToolError::InvalidArgs(_))));
    }

    #[test]
    fn write_rejects_binary_content() {
        let (_temp, registry) = registry();
        let result = registry.execute("write", json!({"path": "blob.bin", "content": "a\u{0}b"}));
        assert!(matches!(result, Err(ToolError::Binary(_))));
    }

    #[test]
    fn mutations_are_marked_and_summarized() {
        let (_temp, registry) = registry();
        assert!(registry.is_mutating("write"));
        assert!(registry.is_mutating("edit"));
        assert!(registry.is_mutating("apply_patch"));
        assert!(!registry.is_mutating("read"));
        assert_eq!(
            registry.summary("write", &json!({"path": "a.txt"})),
            "write a.txt"
        );
    }
}
