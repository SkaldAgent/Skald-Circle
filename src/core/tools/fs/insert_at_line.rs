use anyhow::Result;
use serde_json::{Value, json};

use crate::core::tools::{Tool, ToolDescriptionLength, truncate_label, MAX_LABEL_SHORT, MAX_LABEL_FULL};
use super::{read_to_string, write_string};

pub struct InsertAtLine;

impl InsertAtLine {
    pub fn new() -> Self { Self }
}

impl Tool for InsertAtLine {
    fn name(&self) -> &str { "insert_at_line" }
    fn category(&self) -> crate::core::tools::ToolCategory { crate::core::tools::ToolCategory::Filesystem }

    fn description(&self) -> &str {
        "Insert new text immediately before or after a specific line number in a file. \
         Relative paths are resolved from the project root; absolute paths (starting with /) are used as-is."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path":    { "type": "string",  "description": "File path. Relative to project root, or absolute." },
                "line":    { "type": "integer", "minimum": 1, "description": "1-based line number." },
                "content": { "type": "string",  "description": "Text to insert. May span multiple lines." },
                "placement": {
                    "type": "string",
                    "enum": ["before", "after"],
                    "description": "Whether to insert before or after the target line. Default: \"after\"."
                }
            },
            "required": ["path", "line", "content"]
        })
    }

    fn target_path(&self, args: &Value) -> Option<String> {
        super::path_arg(args)
    }

    fn describe(&self, args: &Value, length: ToolDescriptionLength) -> String {
        let path = args["path"].as_str().unwrap_or("?");
        match length {
            ToolDescriptionLength::Short => {
                truncate_label(&format!("insert_at_line `{path}`"), MAX_LABEL_SHORT)
            }
            ToolDescriptionLength::Full => {
                let line = args["line"].as_u64().map(|n| format!(" line {n}")).unwrap_or_default();
                truncate_label(&format!("insert_at_line `{path}`{line}"), MAX_LABEL_FULL)
            }
        }
    }

    fn execute(&self, args: Value) -> Result<String> {
        let user_path = args["path"].as_str()
            .ok_or_else(|| anyhow::anyhow!("Missing required argument: path"))?;
        let line_num = args["line"].as_u64()
            .ok_or_else(|| anyhow::anyhow!("Missing required argument: line"))? as usize;
        let new_text = args["content"].as_str()
            .ok_or_else(|| anyhow::anyhow!("Missing required argument: content"))?;
        let placement = args["placement"].as_str().unwrap_or("after");

        anyhow::ensure!(line_num >= 1, "line must be >= 1");

        let text = read_to_string(user_path)?;
        let mut lines: Vec<&str> = text.split('\n').collect();
        let idx        = (line_num - 1).min(lines.len().saturating_sub(1));
        let insert_idx = if placement == "before" { idx } else { idx + 1 };
        let new_lines: Vec<&str> = new_text.split('\n').collect();
        for (i, l) in new_lines.iter().enumerate() {
            lines.insert(insert_idx + i, l);
        }
        let updated = lines.join("\n");

        write_string(user_path, &updated)?;

        Ok(format!(
            "Inserted {} line(s) {} line {} in {user_path}.",
            new_lines.len(), placement, line_num
        ))
    }
}
