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

/// A structured LLM call failure carrying the HTTP `status` of the response.
///
/// Clients that read the status themselves (rather than via `error_for_status`)
/// return this so callers can classify retriability on the numeric code instead of
/// substring-matching a formatted message — which mis-fires when a model id, token
/// count or URL merely contains "401"/"404"/… (bug B6). Non-HTTP failures (network,
/// JSON parse, cancellation) stay ordinary `anyhow` errors with no status.
#[derive(Debug)]
pub struct LlmError {
    /// HTTP status code, when the failure came from an HTTP response.
    pub status:  Option<u16>,
    /// Human-readable detail (provider tag + body), used for logs and the UI.
    pub message: String,
}

impl std::fmt::Display for LlmError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for LlmError {}

/// Extracts the HTTP status of an LLM failure, if any: a structured
/// [`LlmError::status`] first, else any `reqwest::Error` in the source chain (the
/// clients that fail via `error_for_status()?`). Returns `None` for a non-HTTP
/// error (network, parse, cancellation), which callers should treat as retriable.
pub fn http_status(err: &anyhow::Error) -> Option<u16> {
    for cause in err.chain() {
        if let Some(le) = cause.downcast_ref::<LlmError>() {
            return le.status;
        }
        if let Some(re) = cause.downcast_ref::<reqwest::Error>() {
            if let Some(s) = re.status() {
                return Some(s.as_u16());
            }
        }
    }
    None
}
