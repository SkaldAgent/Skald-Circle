use std::sync::Arc;

use anyhow::Result;
use serde_json::{Value, json};

use crate::core::secrets::{SecretsApi, SecretsStore};
use crate::core::tools::{Tool, ToolDescriptionLength};

pub struct ListSecrets(pub Arc<SecretsStore>);

impl Tool for ListSecrets {
    fn name(&self) -> &str { "list_secrets" }
    fn category(&self) -> crate::core::tools::ToolCategory { crate::core::tools::ToolCategory::Config }

    fn description(&self) -> &str {
        "List the names (not values) of stored secrets. \
         Optionally filter by glob pattern (e.g. 'GOOGLE_*', 'HF_*'). \
         Returns only keys that are currently set. \
         If a key you expect is absent from the result it has not been configured yet."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "Optional glob pattern to filter key names (e.g. 'GOOGLE_*'). Omit to list all keys."
                }
            }
        })
    }

    fn describe(&self, args: &Value, _length: ToolDescriptionLength) -> String {
        match args["pattern"].as_str() {
            Some(pat) => format!("list secrets ({pat})"),
            None      => "list secrets".to_string(),
        }
    }

    fn execute(&self, args: Value) -> Result<String> {
        let pattern = args["pattern"].as_str();

        let keys = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(self.0.list_keys())
        });

        let filtered: Vec<&str> = match pattern {
            None => keys.iter().map(String::as_str).collect(),
            Some(pat) => keys.iter()
                .filter(|k| glob_match(pat, k))
                .map(String::as_str)
                .collect(),
        };

        Ok(serde_json::to_string_pretty(&filtered)?)
    }
}

/// Minimal glob: `*` matches any sequence of characters, everything else is literal.
fn glob_match(pattern: &str, text: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();
    if parts.len() == 1 {
        return pattern == text;
    }
    let mut remaining = text;
    for (i, part) in parts.iter().enumerate() {
        if part.is_empty() { continue; }
        if i == 0 {
            if !remaining.starts_with(part) { return false; }
            remaining = &remaining[part.len()..];
        } else if i == parts.len() - 1 {
            return remaining.ends_with(part);
        } else {
            match remaining.find(part) {
                None => return false,
                Some(pos) => remaining = &remaining[pos + part.len()..],
            }
        }
    }
    true
}
