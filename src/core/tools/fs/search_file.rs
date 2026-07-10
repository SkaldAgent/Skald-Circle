use anyhow::Result;
use serde_json::{Value, json};

use crate::core::tools::{Tool, ToolDescriptionLength, truncate_label, MAX_LABEL_SHORT, MAX_LABEL_FULL};
use super::read_to_string;

pub struct SearchFile;

impl SearchFile {
    pub fn new() -> Self { Self }
}

impl Tool for SearchFile {
    fn name(&self) -> &str { "search_file" }
    fn category(&self) -> crate::core::tools::ToolCategory { crate::core::tools::ToolCategory::Filesystem }

    fn description(&self) -> &str {
        "Search for lines containing a substring in a file. \
         Relative paths are resolved from the project root; absolute paths (starting with /) are used as-is. \
         Returns each matching line with context, prefixed with 1-based line numbers in '  N | ' format."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path":  { "type": "string", "description": "File path. Relative to project root, or absolute." },
                "query": { "type": "string", "description": "Substring to search for (case-insensitive)." },
                "context_lines": {
                    "type":    "integer",
                    "description": "Lines of context above and below each match (default 3, max 10).",
                    "default": 3
                }
            },
            "required": ["path", "query"]
        })
    }

    fn target_path(&self, args: &Value) -> Option<String> {
        super::path_arg(args)
    }

    fn describe(&self, args: &Value, length: ToolDescriptionLength) -> String {
        let path = args["path"].as_str().unwrap_or("?");
        match length {
            ToolDescriptionLength::Short => {
                truncate_label(&format!("search_file `{path}`"), MAX_LABEL_SHORT)
            }
            ToolDescriptionLength::Full => {
                let query = args["query"].as_str().unwrap_or("?");
                truncate_label(&format!("search_file `{path}` for \"{query}\""), MAX_LABEL_FULL)
            }
        }
    }

    fn execute(&self, args: Value) -> Result<String> {
        let user_path = args["path"].as_str()
            .ok_or_else(|| anyhow::anyhow!("Missing required argument: path"))?;
        let query = args["query"].as_str()
            .ok_or_else(|| anyhow::anyhow!("Missing required argument: query"))?;
        let context = args["context_lines"].as_u64().unwrap_or(3).min(10) as usize;

        let text = read_to_string(user_path)?;
        let lines: Vec<&str> = text.lines().collect();
        let lower_query = query.to_lowercase();
        let width = lines.len().to_string().len().max(3);

        let matches: Vec<usize> = lines.iter().enumerate()
            .filter(|(_, l)| l.to_lowercase().contains(&lower_query))
            .map(|(i, _)| i)
            .collect();

        if matches.is_empty() {
            return Ok(format!("No matches found for {:?} in {user_path}.", query));
        }

        let mut chunks: Vec<(usize, usize)> = Vec::new();
        for &m in &matches {
            let start = m.saturating_sub(context);
            let end   = (m + context).min(lines.len() - 1);
            if let Some(last) = chunks.last_mut() {
                if start <= last.1 + 1 { last.1 = last.1.max(end); continue; }
            }
            chunks.push((start, end));
        }

        let match_set: std::collections::HashSet<usize> = matches.into_iter().collect();
        let mut out = format!("{} match(es) in {user_path}:\n", match_set.len());

        for (ci, (start, end)) in chunks.iter().enumerate() {
            if ci > 0 { out.push_str("  ···\n"); }
            for idx in *start..=*end {
                let marker = if match_set.contains(&idx) { ">" } else { " " };
                out.push_str(&format!("{marker}{:>width$} | {}\n", idx + 1, lines[idx]));
            }
        }

        Ok(out)
    }
}
