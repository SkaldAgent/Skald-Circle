use std::sync::Arc;

use anyhow::Result;
use serde_json::{Value, json};

use crate::core::secrets::{SecretsApi, SecretsStore};
use crate::core::tools::{Tool, ToolDescriptionLength};

pub struct SetSecret(pub Arc<SecretsStore>);

impl Tool for SetSecret {
    fn name(&self) -> &str { "set_secret" }
    fn category(&self) -> crate::core::tools::ToolCategory { crate::core::tools::ToolCategory::Config }

    fn description(&self) -> &str {
        "Store a secret value by key (e.g. HUGGINGFACE_TOKEN). \
         If value is an empty string or null the key is deleted. \
         Secrets are never returned by any tool — use list_secrets to check presence. \
         Keys are uppercase by convention (e.g. HUGGINGFACE_TOKEN, GMAPS_API_KEY)."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "key": {
                    "type": "string",
                    "description": "Secret key name, uppercase (e.g. HUGGINGFACE_TOKEN)."
                },
                "value": {
                    "type": ["string", "null"],
                    "description": "Secret value. Empty string or null deletes the key."
                }
            },
            "required": ["key"]
        })
    }

    fn describe(&self, args: &Value, _length: ToolDescriptionLength) -> String {
        let key = args["key"].as_str().unwrap_or("?");
        format!("set secret {key}")
    }

    fn execute(&self, args: Value) -> Result<String> {
        let key = args["key"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("set_secret: missing required argument `key`"))?;

        let value = args["value"].as_str();

        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                match value {
                    Some(v) if !v.is_empty() => {
                        self.0.set(key, v).await?;
                        Ok(format!("Secret '{key}' set."))
                    }
                    _ => {
                        self.0.delete(key).await?;
                        Ok(format!("Secret '{key}' deleted."))
                    }
                }
            })
        })
    }
}
