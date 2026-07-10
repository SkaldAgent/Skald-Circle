use anyhow::Result;
use serde_json::{Value, json};

use super::tool_names as tn;
use super::{Tool, ToolCategory, ToolDescriptionLength};

pub struct ReadNotification;

impl Tool for ReadNotification {
    fn name(&self) -> &str {
        tn::READ_NOTIFICATION
    }

    fn description(&self) -> &str {
        "Read any pending notifications forwarded by background agents. Returns a JSON array of \
         structured notification objects, each `{source, event_type, summary, event_time, refs}` \
         where `summary` is a neutral, third-person statement of fact. Present the relevant ones to \
         the user in your own voice — always name the source (email, WhatsApp, calendar, …) and add \
         context; do not echo the raw summary as if the user already knew about it."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {},
            "required": []
        })
    }

    fn describe(&self, _args: &Value, _length: ToolDescriptionLength) -> String {
        "read notifications".to_string()
    }

    fn execute(&self, _args: Value) -> Result<String> {
        Ok("[]".to_string())
    }

    fn category(&self) -> ToolCategory {
        ToolCategory::Introspection
    }

    fn root_agent_only(&self) -> bool {
        true
    }

    fn interactive_only(&self) -> bool {
        true
    }
}
