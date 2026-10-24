//! Subagent tool declaration. The runtime executes the child loop.

use serde_json::{Value, json};

use crate::error::ToolError;
use crate::path_jail::PathJail;
use crate::tools::{Tool, ToolDefinition, ToolResult};

/// Declares the bounded `explore`/`general` subagent tool.
pub struct SubagentTool;

impl Tool for SubagentTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "subagent".to_owned(),
            description:
                "Run one bounded child agent. `explore` is read-only; `general` may build and is verified. Depth is limited to one."
                    .to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "kind": {"type": "string", "enum": ["explore", "general"]},
                    "task": {"type": "string", "minLength": 1}
                },
                "required": ["kind", "task"],
                "additionalProperties": false
            }),
        }
    }

    fn execute(&self, _args: Value, _jail: &PathJail) -> Result<ToolResult, ToolError> {
        Err(ToolError::InvalidArgs(
            "subagent is executed by the runtime".to_owned(),
        ))
    }

    fn summary(&self, args: &Value) -> String {
        let kind = args
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or("explore");
        let task = args.get("task").and_then(Value::as_str).unwrap_or("");
        format!(
            "subagent {kind}: {}",
            task.chars().take(80).collect::<String>()
        )
    }
}
