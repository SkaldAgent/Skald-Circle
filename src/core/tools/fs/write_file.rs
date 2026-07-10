use anyhow::Result;
use serde_json::{Value, json};

use crate::core::tools::{Tool, ToolDescriptionLength, truncate_label, MAX_LABEL_SHORT};
use super::{resolve, write_string};

pub struct WriteFile;

impl WriteFile {
    pub fn new() -> Self { Self }
}

impl Tool for WriteFile {
    fn name(&self) -> &str { "write_file" }
    fn category(&self) -> crate::core::tools::ToolCategory { crate::core::tools::ToolCategory::Filesystem }

    fn description(&self) -> &str {
        "Create a new file or fully overwrite an existing one. \
         Use instead of echo/cat heredoc in the terminal. \
         Relative paths are resolved from the project root; absolute paths (starting with /) are used as-is. \
         OVERWRITES the entire file — for targeted edits to an existing file use edit_file instead."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type":        "string",
                    "description": "File path. Relative to project root, or absolute."
                },
                "content": {
                    "type":        "string",
                    "description": "Full content to write to the file."
                }
            },
            "required": ["path", "content"]
        })
    }

    fn target_path(&self, args: &Value) -> Option<String> {
        super::path_arg(args)
    }

    fn describe(&self, args: &Value, length: ToolDescriptionLength) -> String {
        let path = args["path"].as_str().unwrap_or("?");
        let _ = length;
        truncate_label(&format!("write_file `{path}`"), MAX_LABEL_SHORT)
    }

    fn execute(&self, args: Value) -> Result<String> {
        let user_path = args["path"].as_str()
            .ok_or_else(|| anyhow::anyhow!("Missing required argument: path"))?;
        let content = args["content"].as_str()
            .ok_or_else(|| anyhow::anyhow!("Missing required argument: content"))?;

        let abs = resolve(user_path)?;
        let existed = abs.exists();
        write_string(user_path, content)?;

        if existed {
            Ok(format!("Overwrote {user_path} ({} bytes).", content.len()))
        } else {
            Ok(format!("Created {user_path} ({} bytes).", content.len()))
        }
    }
}
