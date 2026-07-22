pub mod anthropic;
pub mod lm_studio;
pub mod ollama;
pub mod openai;

// Re-export the trait and all associated types from core-api so existing
// callers that import from `llm_client` continue to work unchanged.
pub use core_api::chatbot::{
    ChatOptions, ChatResponse, ChatbotClient, LlmRawMeta, LlmTurn, Message, Role, StreamDelta,
    ToolCall,
};

use serde_json::Value;

/// Incremental SSE decoder: feed raw response bytes, get back the payload of
/// every complete `data:` line seen (`[DONE]` included — callers decide).
/// Buffers partial lines across chunks; `event:` lines and comments are
/// skipped (both OpenAI and Anthropic put the event type inside the JSON).
#[derive(Default)]
pub struct SseDecoder {
    buf: Vec<u8>,
}

impl SseDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn feed(&mut self, bytes: &[u8]) -> Vec<String> {
        self.buf.extend_from_slice(bytes);
        let mut out = Vec::new();
        while let Some(pos) = self.buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.buf.drain(..=pos).collect();
            if let Some(payload) = parse_sse_line(&line) {
                out.push(payload);
            }
        }
        out
    }

    /// Flush a trailing line not terminated by `\n` at end-of-stream.
    pub fn finish(&mut self) -> Vec<String> {
        let rest = std::mem::take(&mut self.buf);
        parse_sse_line(&rest).into_iter().collect()
    }
}

/// A complete SSE line is valid UTF-8 (a multibyte sequence never contains a
/// `\n` byte), but decode lossily anyway — a corrupt line is skipped, not fatal.
fn parse_sse_line(line: &[u8]) -> Option<String> {
    let line = String::from_utf8_lossy(line);
    let line = line.trim_end_matches('\r').trim();
    let data = line.strip_prefix("data:")?.trim_start();
    if data.is_empty() { None } else { Some(data.to_string()) }
}

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

/// Turns a raw error-response body into a JSON `Value` for the payload log:
/// the parsed JSON when the provider returned JSON (the common case — an
/// `{"error": …}` object), else the raw text wrapped as a JSON string so a
/// non-JSON body (HTML gateway page, plain text) is still preserved verbatim.
pub fn error_response_body(text: String) -> Value {
    serde_json::from_str::<Value>(&text).unwrap_or(Value::String(text))
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
#[derive(Debug, Default)]
pub struct LlmError {
    /// HTTP status code, when the failure came from an HTTP response.
    pub status:  Option<u16>,
    /// Human-readable detail (provider tag + body), used for logs and the UI.
    pub message: String,
    /// Request/response payload captured at the failing call, so the debug log
    /// can show what was actually sent even when the provider rejected it (e.g.
    /// a 400). `None` for failures with no HTTP round-trip (network, cancellation,
    /// parse) — those carry no body to surface.
    pub raw_meta: Option<LlmRawMeta>,
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

#[cfg(test)]
mod tests {
    use super::SseDecoder;

    #[test]
    fn sse_decoder_buffers_partial_lines_across_chunks() {
        let mut dec = SseDecoder::new();
        // A payload split mid-JSON across two chunks yields one complete line.
        assert!(dec.feed(br#"data: {"a": 1"#).is_empty());
        assert_eq!(dec.feed(b"}\r\n").len(), 1);
    }

    #[test]
    fn sse_decoder_skips_events_comments_and_keeps_done() {
        let mut dec = SseDecoder::new();
        let out = dec.feed(b"event: message_start\n: ping\n\ndata: {\"type\":\"ping\"}\ndata: [DONE]\n");
        assert_eq!(out, vec!["{\"type\":\"ping\"}".to_string(), "[DONE]".to_string()]);
        assert!(dec.finish().is_empty());
    }

    #[test]
    fn sse_decoder_finish_flushes_unterminated_tail() {
        let mut dec = SseDecoder::new();
        assert!(dec.feed(b"data: tail-without-newline").is_empty());
        assert_eq!(dec.finish(), vec!["tail-without-newline".to_string()]);
    }

    #[test]
    fn sse_decoder_handles_multibyte_split() {
        let mut dec = SseDecoder::new();
        // "€" is 3 bytes in UTF-8; split across the chunk boundary.
        let payload = "data: {\"t\":\"€\"}\n".as_bytes();
        let (a, b) = payload.split_at(12);
        assert!(dec.feed(a).is_empty());
        assert_eq!(dec.feed(b), vec!["{\"t\":\"€\"}".to_string()]);
    }
}
