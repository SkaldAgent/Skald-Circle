use anyhow::Result;
use serde_json::{Value, json};

use crate::core::tools::{Tool, ToolDescriptionLength, truncate_label, MAX_LABEL_SHORT, MAX_LABEL_FULL};
use super::read_to_string;

pub struct ReadFile;

impl ReadFile {
    pub fn new() -> Self { Self }
}

impl Tool for ReadFile {
    fn name(&self) -> &str { "read_file" }
    fn category(&self) -> crate::core::tools::ToolCategory { crate::core::tools::ToolCategory::Filesystem }

    fn description(&self) -> &str {
        "Read the content of a file with 1-based line numbers. \
         Use instead of cat/head/tail in the terminal. \
         Returns text prefixed as '  N | line'. When calling edit_file, copy the text after '| ' exactly. \
         For large files use start_line/end_line to read in chunks — files over ~2000 lines should never be read whole. \
         Use limit to cap output when end_line is unknown."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type":        "string",
                    "description": "File path. Relative to project root, or absolute (e.g. /etc/hosts)."
                },
                "start_line": {
                    "type":        "integer",
                    "description": "First line to read (1-based, inclusive). Omit to start from the beginning."
                },
                "end_line": {
                    "type":        "integer",
                    "description": "Last line to read (1-based, inclusive). Omit to read to the end of the file."
                },
                "limit": {
                    "type":        "integer",
                    "description": "Maximum number of lines to return (max 2000). Applied after start_line when end_line is omitted.",
                    "maximum":     2000
                }
            },
            "required": ["path"]
        })
    }

    fn target_path(&self, args: &Value) -> Option<String> {
        super::path_arg(args)
    }

    fn describe(&self, args: &Value, length: ToolDescriptionLength) -> String {
        let path = args["path"].as_str().unwrap_or("?");
        match length {
            ToolDescriptionLength::Short => {
                truncate_label(&format!("read_file `{path}`"), MAX_LABEL_SHORT)
            }
            ToolDescriptionLength::Full => {
                let range = match (args["start_line"].as_u64(), args["end_line"].as_u64()) {
                    (Some(s), Some(e)) => format!(" lines {s}-{e}"),
                    (Some(s), None)    => format!(" from line {s}"),
                    (None,    Some(e)) => format!(" to line {e}"),
                    _                  => String::new(),
                };
                truncate_label(&format!("read_file `{path}`{range}"), MAX_LABEL_FULL)
            }
        }
    }

    fn execute(&self, args: Value) -> Result<String> {
        let user_path = args["path"].as_str()
            .ok_or_else(|| anyhow::anyhow!("Missing required argument: path"))?;
        let content = read_to_string(user_path)?;
        let lines: Vec<&str> = content.lines().collect();
        let total = lines.len();

        let limit = args["limit"].as_u64().map(|n| n.min(2000) as usize);
        let start = args["start_line"].as_u64()
            .map(|n| (n as usize).saturating_sub(1))
            .unwrap_or(0);
        let end = match (args["end_line"].as_u64(), limit) {
            (Some(e), _)    => (e as usize).min(total),
            (None, Some(l)) => (start + l).min(total),
            (None, None)    => total,
        };

        if start >= total && total > 0 {
            return Ok(format!("(file has only {total} lines; start_line {start_line} is out of range)",
                start_line = start + 1));
        }

        let end = end.max(start);

        let width = total.to_string().len().max(3);
        let numbered = lines[start..end]
            .iter()
            .enumerate()
            .map(|(i, line)| format!("{:>width$} | {line}", start + i + 1))
            .collect::<Vec<_>>()
            .join("\n");
        Ok(numbered)
    }
}
