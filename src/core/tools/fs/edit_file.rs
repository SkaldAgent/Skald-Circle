use anyhow::Result;
use serde_json::{Value, json};

use crate::core::tools::{Tool, ToolDescriptionLength, truncate_label, MAX_LABEL_SHORT};
use super::{read_to_string, write_string};

fn normalize_ws(s: &str) -> String {
    s.lines()
        .map(|l| {
            let mut out = String::with_capacity(l.len());
            let mut last_space = true;
            for ch in l.chars() {
                if ch.is_whitespace() {
                    if !last_space { out.push(' '); }
                    last_space = true;
                } else {
                    out.push(ch);
                    last_space = false;
                }
            }
            out.trim_end().to_owned()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn find_normalized(haystack: &str, normalized_needle: &str) -> Option<(usize, usize)> {
    let needle_lines: Vec<&str> = normalized_needle.lines().collect();
    let n = needle_lines.len();
    if n == 0 { return None; }

    let hay_lines: Vec<&str> = haystack.lines().collect();
    let hay_count = hay_lines.len();

    let mut offsets = Vec::with_capacity(hay_count + 1);
    offsets.push(0usize);
    for line in &hay_lines {
        let prev = *offsets.last().unwrap();
        offsets.push(prev + line.len() + 1);
    }

    for start_idx in 0..=(hay_count.saturating_sub(n)) {
        let matches = (0..n).all(|i| {
            normalize_ws(hay_lines[start_idx + i]).as_str() == needle_lines[i]
        });
        if matches {
            let byte_start = offsets[start_idx];
            let byte_end = if start_idx + n < hay_count {
                offsets[start_idx + n]
            } else {
                haystack.len()
            };
            return Some((byte_start, byte_end));
        }
    }
    None
}

pub struct EditFile;

impl EditFile {
    pub fn new() -> Self { Self }
}

impl Tool for EditFile {
    fn name(&self) -> &str { "edit_file" }
    fn category(&self) -> crate::core::tools::ToolCategory { crate::core::tools::ToolCategory::Filesystem }

    fn description(&self) -> &str {
        "Replace a substring in a file with new text. \
         Use instead of sed/awk in the terminal. \
         Relative paths are resolved from the project root; absolute paths (starting with /) are used as-is. \
         By default `old` must be unique — include enough surrounding context to make it so. \
         Always call read_file first and copy text exactly as shown after '| ' (the '  N | ' prefix is NOT part of the file). \
         Set replace_all=true to replace every occurrence instead of requiring uniqueness."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "File path. Relative to project root, or absolute." },
                "old":  { "type": "string", "description": "Text to find and replace. Must be unique in the file unless replace_all=true." },
                "new":  { "type": "string", "description": "Replacement text. Pass empty string to delete the matched text." },
                "replace_all": {
                    "type": "boolean",
                    "description": "Replace every occurrence of old instead of requiring a unique match (default: false).",
                    "default": false
                }
            },
            "required": ["path", "old", "new"]
        })
    }

    fn target_path(&self, args: &Value) -> Option<String> {
        super::path_arg(args)
    }

    fn describe(&self, args: &Value, length: ToolDescriptionLength) -> String {
        let path = args["path"].as_str().unwrap_or("?");
        let _ = length;
        truncate_label(&format!("edit_file `{path}`"), MAX_LABEL_SHORT)
    }

    fn execute(&self, args: Value) -> Result<String> {
        let user_path = args["path"].as_str()
            .ok_or_else(|| anyhow::anyhow!("Missing required argument: path"))?;
        let old = args["old"].as_str()
            .ok_or_else(|| anyhow::anyhow!("Missing required argument: old"))?;
        let new = args["new"].as_str()
            .ok_or_else(|| anyhow::anyhow!("Missing required argument: new"))?;

        let replace_all = args["replace_all"].as_bool().unwrap_or(false);
        let content = read_to_string(user_path)?;

        let updated = if replace_all {
            if !content.contains(old) {
                anyhow::bail!(
                    "Text not found in {user_path}. \
                     Call read_file first and copy the text exactly as shown after the '| ' prefix."
                );
            }
            content.replace(old, new)
        } else {
            let exact_count = content.matches(old).count();
            if exact_count > 1 {
                anyhow::bail!(
                    "Text found {exact_count} times in {user_path}. \
                     Include more surrounding context in `old` to make it unique, or set replace_all=true."
                );
            }
            if exact_count == 1 {
                content.replacen(old, new, 1)
            } else {
                let normalized_old = normalize_ws(old);
                let (start, end) = find_normalized(&content, &normalized_old)
                    .ok_or_else(|| anyhow::anyhow!(
                        "Text not found in {user_path}. \
                         Call read_file first and copy the text exactly as shown after the '| ' prefix."
                    ))?;
                format!("{}{}{}", &content[..start], new, &content[end..])
            }
        };

        write_string(user_path, &updated)?;
        Ok(format!("Edited {user_path}."))
    }
}
