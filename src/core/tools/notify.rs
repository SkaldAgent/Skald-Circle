use std::sync::Arc;

use serde_json::{Value, json};

use crate::core::chat_hub::ChatHub;
use crate::core::notification::Notification;
use crate::core::session::handler::{InterfaceTool, ToolFuture};

/// Build a `notify` InterfaceTool bound to the given `ChatHub`.
///
/// `default_source` is used as the notification `source` only when the caller
/// omits one (kept for callers like TIC that pass a fixed origin tag). Normally
/// the agent supplies `source` explicitly from the event it is surfacing.
pub fn make_tool(hub: Arc<ChatHub>, default_source: impl Into<String>) -> InterfaceTool {
    let default_source = default_source.into();
    let definition = json!({
        "type": "function",
        "function": {
            "name": crate::core::tools::tool_names::NOTIFY,
            "description": "Surface a single event to the user's home conversation as a structured \
                            notification. Call once per event worth surfacing. Provide factual, \
                            third-person data about the event — do NOT write a message to the user; \
                            the main agent composes the user-facing wording from these fields.",
            "parameters": {
                "type": "object",
                "properties": {
                    "source": {
                        "type": "string",
                        "enum": ["gmail", "whatsapp", "gcal", "cron", "system"],
                        "description": "Where the event originated."
                    },
                    "event_type": {
                        "type": "string",
                        "description": "Kind of event, e.g. \"new_email\", \"whatsapp_message\", \"new_calendar_event\"."
                    },
                    "summary": {
                        "type": "string",
                        "description": "Neutral, third-person factual description of the event (NOT a message to the \
                                        user). Name the key facts and add relevant context. Plain prose, no markdown."
                    },
                    "event_time": {
                        "type": "string",
                        "description": "ISO 8601 timestamp of the event (copy it from the event's Received time)."
                    },
                    "refs": {
                        "type": "object",
                        "description": "Actionable references pulled from the event payload (e.g. message_id, thread_id, \
                                        from, event_id). Lets the main agent act on the event later.",
                        "additionalProperties": true
                    }
                },
                "required": ["source", "summary"]
            }
        }
    });

    let handler = Arc::new(move |args: Value| -> ToolFuture {
        let hub            = Arc::clone(&hub);
        let default_source = default_source.clone();
        Box::pin(async move {
            let summary = args["summary"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("notify: missing required parameter 'summary'"))?
                .to_string();
            let source = args["source"]
                .as_str()
                .map(str::to_string)
                .unwrap_or_else(|| default_source.clone());
            let event_type = args["event_type"].as_str().unwrap_or("").to_string();
            let event_time = args["event_time"]
                .as_str()
                .map(str::to_string)
                .unwrap_or_else(|| chrono::Utc::now().to_rfc3339());
            let refs = args.get("refs").cloned().unwrap_or_else(|| json!({}));

            hub.notify(Notification { source, event_type, summary, event_time, refs }).await?;
            Ok("Notification queued.".to_string())
        })
    });

    InterfaceTool { definition, handler }
}
