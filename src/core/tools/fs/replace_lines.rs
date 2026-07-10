use anyhow::Result;
use serde_json::{Value, json};

use crate::core::tools::{Tool, ToolDescriptionLength, truncate_label, MAX_LABEL_SHORT, MAX_LABEL_FULL};
use super::{read_to_string, write_string};

pub struct ReplaceLines;

impl ReplaceLines {
    pub fn new() -> Self { Self }
}

impl Tool for ReplaceLines {
    fn name(&self) -> &str { "replace_lines" }
    fn category(&self) -> crate::core::tools::ToolCategory { crate::core::tools::ToolCategory::Filesystem }

    fn description(&self) -> &str {
        "Replace a range of lines in a file with new text. \
         Relative paths are resolved from the project root; absolute paths (starting with /) are used as-is. \
         Use the 1-based line numbers shown by read_file. `from_line` and `to_line` are inclusive."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path":      { "type": "string",  "description": "File path. Relative to project root, or absolute." },
                "from_line": { "type": "integer", "description": "First line to replace (1-based, inclusive)." },
                "to_line":   { "type": "integer", "description": "Last line to replace (1-based, inclusive)." },
                "new":       { "type": "string",  "description": "Replacement text." }
            },
            "required": ["path", "from_line", "to_line", "new"]
        })
    }

    fn target_path(&self, args: &Value) -> Option<String> {
        super::path_arg(args)
    }

    fn describe(&self, args: &Value, length: ToolDescriptionLength) -> String {
        let path = args["path"].as_str().unwrap_or("?");
        match length {
            ToolDescriptionLength::Short => {
                truncate_label(&format!("replace_lines `{path}`"), MAX_LABEL_SHORT)
            }
            ToolDescriptionLength::Full => {
                let from = args["from_line"].as_u64().map(|n| n.to_string()).unwrap_or_else(|| "?".into());
                let to   = args["to_line"].as_u64().map(|n| n.to_string()).unwrap_or_else(|| "?".into());
                truncate_label(&format!("replace_lines `{path}` lines {from}-{to}"), MAX_LABEL_FULL)
            }
        }
    }

    fn execute(&self, args: Value) -> Result<String> {
        let user_path = args["path"].as_str()
            .ok_or_else(|| anyhow::anyhow!("Missing required argument: path"))?;
        let from_line = args["from_line"].as_u64()
            .ok_or_else(|| anyhow::anyhow!("Missing required argument: from_line"))? as usize;
        let to_line = args["to_line"].as_u64()
            .ok_or_else(|| anyhow::anyhow!("Missing required argument: to_line"))? as usize;
        let new = args["new"].as_str()
            .ok_or_else(|| anyhow::anyhow!("Missing required argument: new"))?;

        if from_line == 0 { anyhow::bail!("from_line must be >= 1"); }
        if to_line < from_line { anyhow::bail!("to_line must be >= from_line"); }

        let content = read_to_string(user_path)?;
        let mut lines: Vec<&str> = content.lines().collect();
        let total = lines.len();
        if from_line > total {
            anyhow::bail!("from_line {from_line} exceeds file length ({total} lines)");
        }
        let to_clamped = to_line.min(total);
        let new_lines: Vec<&str> = new.lines().collect();
        lines.splice((from_line - 1)..to_clamped, new_lines);

        let has_trailing = content.ends_with('\n');
        let mut updated = lines.join("\n");
        if has_trailing { updated.push('\n'); }

        write_string(user_path, &updated)?;

        Ok(format!(
            "Replaced lines {from_line}–{to_clamped} in {user_path} with {} new lines.",
            new.lines().count()
        ))
    }
}
