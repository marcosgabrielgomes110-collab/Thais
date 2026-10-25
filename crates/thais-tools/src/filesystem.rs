//! Safe workspace filesystem mutations: delete, move, and mkdir.

use std::path::Path;

use serde_json::{Value, json};

use crate::error::ToolError;
use crate::path_jail::PathJail;
use crate::tools::{Tool, ToolDefinition, ToolResult, cap_output};

const MAX_PREVIEW_ENTRIES: usize = 1_000;

/// Delete a file, symlink, or directory inside the workspace.
pub struct DeleteTool;

impl Tool for DeleteTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "delete".to_owned(),
            description:
                "Delete a file, symlink, or directory inside the workspace. Directories require recursive=true. Symlinks are removed, not followed."
                    .to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "minLength": 1},
                    "recursive": {"type": "boolean"}
                },
                "required": ["path"],
                "additionalProperties": false
            }),
        }
    }

    fn mutating(&self) -> bool {
        true
    }

    fn summary(&self, args: &Value) -> String {
        format!(
            "delete {}",
            args.get("path").and_then(Value::as_str).unwrap_or("")
        )
    }

    fn preview(&self, args: &Value, jail: &PathJail) -> Option<String> {
        let path = jail.resolve_mutation(args.get("path")?.as_str()?).ok()?;
        let recursive = args
            .get("recursive")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let metadata = std::fs::symlink_metadata(&path).ok()?;
        if metadata.is_dir() && recursive {
            Some(render_tree(&path))
        } else {
            Some(format!("delete {}\n", jail.display(&path)))
        }
    }

    fn execute(&self, args: Value, jail: &PathJail) -> Result<ToolResult, ToolError> {
        let path = jail.resolve_mutation(required_str(&args, "path")?)?;
        let recursive = args
            .get("recursive")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let metadata = match std::fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(ToolError::NotFound(path));
            }
            Err(source) => return Err(ToolError::Io { path, source }),
        };

        if metadata.is_dir() {
            if !recursive {
                let mut entries = std::fs::read_dir(&path).map_err(|source| ToolError::Io {
                    path: path.clone(),
                    source,
                })?;
                if entries.next().is_some() {
                    return Err(ToolError::InvalidArgs(
                        "directory is not empty; pass recursive=true".to_owned(),
                    ));
                }
                std::fs::remove_dir(&path).map_err(|source| ToolError::Io {
                    path: path.clone(),
                    source,
                })?;
            } else {
                std::fs::remove_dir_all(&path).map_err(|source| ToolError::Io {
                    path: path.clone(),
                    source,
                })?;
            }
        } else {
            std::fs::remove_file(&path).map_err(|source| ToolError::Io {
                path: path.clone(),
                source,
            })?;
        }

        Ok(ToolResult::ok(format!("deleted {}\n", jail.display(&path))))
    }
}

/// Move or rename a file, symlink, or directory inside the workspace.
pub struct MoveTool;

impl Tool for MoveTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "move".to_owned(),
            description:
                "Move or rename a file, symlink, or directory inside the workspace. Existing destinations are refused unless overwrite=true."
                    .to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "from": {"type": "string", "minLength": 1},
                    "to": {"type": "string", "minLength": 1},
                    "overwrite": {"type": "boolean"}
                },
                "required": ["from", "to"],
                "additionalProperties": false
            }),
        }
    }

    fn mutating(&self) -> bool {
        true
    }

    fn summary(&self, args: &Value) -> String {
        format!(
            "move {} -> {}",
            args.get("from").and_then(Value::as_str).unwrap_or(""),
            args.get("to").and_then(Value::as_str).unwrap_or("")
        )
    }

    fn preview(&self, args: &Value, jail: &PathJail) -> Option<String> {
        let from = jail.resolve_mutation(args.get("from")?.as_str()?).ok()?;
        let to = jail.resolve_mutation(args.get("to")?.as_str()?).ok()?;
        let overwrite = args
            .get("overwrite")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        Some(format!(
            "move {} -> {}{}\n",
            jail.display(&from),
            jail.display(&to),
            if overwrite { " (overwrite)" } else { "" }
        ))
    }

    fn execute(&self, args: Value, jail: &PathJail) -> Result<ToolResult, ToolError> {
        let from = jail.resolve_mutation(required_str(&args, "from")?)?;
        let to = jail.resolve_mutation(required_str(&args, "to")?)?;
        let overwrite = args
            .get("overwrite")
            .and_then(Value::as_bool)
            .unwrap_or(false);

        if from == to {
            return Err(ToolError::InvalidArgs(
                "source and destination are the same path".to_owned(),
            ));
        }
        match std::fs::symlink_metadata(&from) {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(ToolError::NotFound(from));
            }
            Err(source) => return Err(ToolError::Io { path: from, source }),
        }

        if let Ok(metadata) = std::fs::symlink_metadata(&to) {
            if !overwrite {
                return Err(ToolError::InvalidArgs(format!(
                    "{} already exists; pass overwrite=true",
                    jail.display(&to)
                )));
            }
            if metadata.is_dir() {
                return Err(ToolError::InvalidArgs(
                    "refusing to replace a directory; delete it first".to_owned(),
                ));
            }
            std::fs::remove_file(&to).map_err(|source| ToolError::Io {
                path: to.clone(),
                source,
            })?;
        }

        std::fs::rename(&from, &to).map_err(|source| ToolError::Io {
            path: from.clone(),
            source,
        })?;
        Ok(ToolResult::ok(format!(
            "moved {} -> {}\n",
            jail.display(&from),
            jail.display(&to)
        )))
    }
}

/// Create a directory inside the workspace.
pub struct MkdirTool;

impl Tool for MkdirTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "mkdir".to_owned(),
            description:
                "Create a directory inside the workspace. Parent directories are created by default."
                    .to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "minLength": 1},
                    "parents": {"type": "boolean"}
                },
                "required": ["path"],
                "additionalProperties": false
            }),
        }
    }

    fn mutating(&self) -> bool {
        true
    }

    fn summary(&self, args: &Value) -> String {
        format!(
            "mkdir {}",
            args.get("path").and_then(Value::as_str).unwrap_or("")
        )
    }

    fn preview(&self, args: &Value, jail: &PathJail) -> Option<String> {
        let path = jail.resolve(args.get("path")?.as_str()?).ok()?;
        jail.ensure_mutable(&path).ok()?;
        Some(format!("mkdir {}\n", jail.display(&path)))
    }

    fn execute(&self, args: Value, jail: &PathJail) -> Result<ToolResult, ToolError> {
        let path = jail.resolve(required_str(&args, "path")?)?;
        jail.ensure_mutable(&path)?;
        if let Ok(metadata) = std::fs::symlink_metadata(&path) {
            if metadata.is_dir() {
                return Ok(ToolResult::ok(format!(
                    "{} already exists\n",
                    jail.display(&path)
                )));
            }
            return Err(ToolError::InvalidArgs(format!(
                "{} exists and is not a directory",
                jail.display(&path)
            )));
        }
        let parents = args.get("parents").and_then(Value::as_bool).unwrap_or(true);
        let result = if parents {
            std::fs::create_dir_all(&path)
        } else {
            std::fs::create_dir(&path)
        };
        result.map_err(|source| ToolError::Io {
            path: path.clone(),
            source,
        })?;
        Ok(ToolResult::ok(format!("created {}\n", jail.display(&path))))
    }
}

fn required_str<'a>(args: &'a Value, key: &str) -> Result<&'a str, ToolError> {
    args.get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| ToolError::InvalidArgs(format!("missing `{key}`")))
}

fn render_tree(path: &Path) -> String {
    let mut lines = vec![format!("delete {}\n", path.display())];
    let mut stack = vec![path.to_path_buf()];
    let mut entries = 0;
    while let Some(current) = stack.pop() {
        let Ok(read_dir) = std::fs::read_dir(&current) else {
            continue;
        };
        for entry in read_dir.flatten() {
            if entries >= MAX_PREVIEW_ENTRIES {
                lines.push("[truncated]\n".to_owned());
                return cap_output(lines.concat());
            }
            entries += 1;
            let entry_path = entry.path();
            let metadata = std::fs::symlink_metadata(&entry_path);
            let is_dir = metadata.as_ref().is_ok_and(std::fs::Metadata::is_dir);
            lines.push(format!(
                "{} {}\n",
                if is_dir { "dir " } else { "file" },
                entry_path.display()
            ));
            if is_dir {
                stack.push(entry_path);
            }
        }
    }
    cap_output(lines.concat())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::ToolRegistry;

    fn registry() -> (tempfile::TempDir, ToolRegistry) {
        let temp = tempfile::tempdir().expect("temp");
        let registry = ToolRegistry::read_write(temp.path()).expect("registry");
        (temp, registry)
    }

    #[test]
    fn mkdir_move_and_delete_a_file() {
        let (temp, registry) = registry();
        registry
            .execute("mkdir", json!({"path": "generated/nested"}))
            .expect("mkdir");
        std::fs::write(temp.path().join("source.txt"), "content").expect("file");
        registry
            .execute(
                "move",
                json!({"from": "source.txt", "to": "generated/nested/target.txt"}),
            )
            .expect("move");
        assert!(temp.path().join("generated/nested/target.txt").is_file());
        assert!(!temp.path().join("source.txt").exists());

        registry
            .execute("delete", json!({"path": "generated/nested/target.txt"}))
            .expect("delete");
        assert!(!temp.path().join("generated/nested/target.txt").exists());
    }

    #[test]
    fn delete_requires_recursive_for_non_empty_directories() {
        let (temp, registry) = registry();
        std::fs::create_dir_all(temp.path().join("dir")).expect("dir");
        std::fs::write(temp.path().join("dir/file.txt"), "x").expect("file");
        let error = registry
            .execute("delete", json!({"path": "dir"}))
            .expect_err("non-recursive");
        assert!(error.to_string().contains("recursive=true"));
        assert!(temp.path().join("dir/file.txt").exists());
        registry
            .execute("delete", json!({"path": "dir", "recursive": true}))
            .expect("recursive");
        assert!(!temp.path().join("dir").exists());
    }

    #[test]
    fn move_refuses_overwrite_without_the_flag() {
        let (temp, registry) = registry();
        std::fs::write(temp.path().join("a.txt"), "a").expect("a");
        std::fs::write(temp.path().join("b.txt"), "b").expect("b");
        let error = registry
            .execute("move", json!({"from": "a.txt", "to": "b.txt"}))
            .expect_err("overwrite");
        assert!(error.to_string().contains("overwrite=true"));
        assert_eq!(
            std::fs::read_to_string(temp.path().join("b.txt")).expect("b"),
            "b"
        );
        registry
            .execute(
                "move",
                json!({"from": "a.txt", "to": "b.txt", "overwrite": true}),
            )
            .expect("overwrite");
        assert_eq!(
            std::fs::read_to_string(temp.path().join("b.txt")).expect("b"),
            "a"
        );
    }

    #[cfg(unix)]
    #[test]
    fn delete_removes_a_symlink_without_deleting_its_target() {
        use std::os::unix::fs::symlink;
        let (temp, registry) = registry();
        let outside = tempfile::tempdir().expect("outside");
        std::fs::write(outside.path().join("target.txt"), "safe").expect("target");
        symlink(
            outside.path().join("target.txt"),
            temp.path().join("link.txt"),
        )
        .expect("link");
        registry
            .execute("delete", json!({"path": "link.txt"}))
            .expect("delete");
        assert!(!temp.path().join("link.txt").exists());
        assert!(outside.path().join("target.txt").exists());
    }

    #[test]
    fn protected_paths_are_rejected() {
        let (temp, registry) = registry();
        std::fs::create_dir_all(temp.path().join(".git")).expect("git");
        for tool in ["delete", "move", "mkdir"] {
            let error = match tool {
                "delete" => registry
                    .execute(tool, json!({"path": ".git/config"}))
                    .expect_err("delete"),
                "move" => registry
                    .execute(tool, json!({"from": "a", "to": ".git/config"}))
                    .expect_err("move"),
                _ => registry
                    .execute(tool, json!({"path": ".git/config"}))
                    .expect_err("mkdir"),
            };
            assert!(error.to_string().contains("protected"), "{tool}: {error}");
        }
    }

    #[test]
    fn previews_do_not_mutate() {
        let (temp, registry) = registry();
        std::fs::write(temp.path().join("a.txt"), "a").expect("a");
        let preview = registry.preview("delete", &json!({"path": "a.txt"}));
        assert!(preview.expect("preview").contains("delete a.txt"));
        assert!(temp.path().join("a.txt").exists());
    }
}
