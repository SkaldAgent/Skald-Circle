//! Shipped `Model` clients (blueprint D13): OpenAI-compatible, Anthropic,
//! Ollama, LM Studio — plus the shared SSE decoder and HTTP helpers.
//!
//! All clients are stateless (connection config only) and share the same
//! failure policy: if a stream dies BEFORE any delta, the client retries
//! buffered on the same model (providers rejecting `stream` keep working); a
//! mid-stream failure propagates to the caller's fallback logic.

pub mod anthropic;
pub mod lm_studio;
pub mod ollama;
pub mod openai;
mod sse;

pub use anthropic::AnthropicModel;
pub use lm_studio::LmStudioModel;
pub use ollama::OllamaModel;
pub use openai::OpenAiModel;
pub(crate) use sse::SseDecoder;

use serde_json::Value;

/// Converts a reqwest `HeaderMap` into a JSON object (for payload logging).
pub(crate) fn headers_to_json(headers: &reqwest::header::HeaderMap) -> Value {
    let map: serde_json::Map<String, Value> = headers
        .iter()
        .map(|(k, v)| (k.as_str().to_string(), v.to_str().unwrap_or("<binary>").into()))
        .collect();
    Value::Object(map)
}

/// Raw error body → JSON for the payload log: parsed JSON when the provider
/// returned JSON, else the raw text wrapped as a JSON string so a non-JSON
/// body (HTML gateway page) is still preserved verbatim.
pub(crate) fn error_response_body(text: String) -> Value {
    serde_json::from_str::<Value>(&text).unwrap_or(Value::String(text))
}

/// Redacted preview of an API key: first 7 chars + "***".
pub(crate) fn redact_key(key: &str) -> String {
    if key.len() > 7 { format!("{}***", &key[..7]) } else { "***".to_string() }
}
