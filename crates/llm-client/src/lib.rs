pub mod anthropic;
pub mod lm_studio;
pub mod ollama;
pub mod openai;

// Re-export the trait and all associated types from core-api so existing
// callers that import from `llm_client` continue to work unchanged.
pub use core_api::chatbot::{
    ChatOptions, ChatResponse, ChatbotClient, LlmRawMeta, LlmTurn, Message, Role, ToolCall,
};

use serde_json::Value;

/// Converts a reqwest `HeaderMap` into a `serde_json::Value` object.
pub fn headers_to_json(headers: &reqwest::header::HeaderMap) -> Value {
    let map: serde_json::Map<String, Value> = headers
        .iter()
        .map(|(k, v)| (
            k.as_str().to_string(),
            v.to_str().unwrap_or("<binary>").into(),
        ))
        .collect();
    Value::Object(map)
}

/// Returns a redacted preview of an API key: first 7 chars + "***".
pub fn redact_key(key: &str) -> String {
    if key.len() > 7 {
        format!("{}***", &key[..7])
    } else {
        "***".to_string()
    }
}
