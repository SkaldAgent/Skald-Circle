use async_trait::async_trait;
use serde_json::{Value, json};
use tracing::{debug, info, trace, warn};

use crate::{ChatOptions, ChatResponse, ChatbotClient, LlmRawMeta, LlmTurn, Message, Role, ToolCall, headers_to_json, redact_key};

const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
const ANTHROPIC_VERSION: &str = "2023-06-01";

pub struct AnthropicClient {
    base_url:   String,
    api_key:    String,
    /// Extra top-level request-body keys merged into every request (e.g. the
    /// `thinking` config for extended reasoning). See `apply_extra`.
    extra_body: Option<Value>,
    http:       reqwest::Client,
}

impl AnthropicClient {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self::with_base_url(DEFAULT_BASE_URL, api_key)
    }

    pub fn with_base_url(base_url: impl Into<String>, api_key: impl Into<String>) -> Self {
        Self {
            base_url:   base_url.into(),
            api_key:    api_key.into(),
            extra_body: None,
            http:       reqwest::Client::new(),
        }
    }

    /// Like `new` but with extra request-body keys (e.g. `{"thinking": {...}}`).
    pub fn with_extra_body(api_key: impl Into<String>, extra_body: Option<Value>) -> Self {
        Self {
            base_url:   DEFAULT_BASE_URL.to_string(),
            api_key:    api_key.into(),
            extra_body,
            http:       reqwest::Client::new(),
        }
    }

    /// Merges `extra_body` into `body` and enforces Anthropic's extended-thinking
    /// constraints: when `thinking` is enabled, `temperature` is not allowed and
    /// `max_tokens` must be strictly greater than `budget_tokens`.
    fn apply_extra(&self, body: &mut Value) {
        let Some(extra) = self.extra_body.as_ref().and_then(|v| v.as_object()) else { return };
        let Some(obj) = body.as_object_mut() else { return };
        for (k, v) in extra {
            obj.insert(k.clone(), v.clone());
        }
        if obj.get("thinking").map(|t| t["type"] == json!("enabled")).unwrap_or(false) {
            obj.remove("temperature");
            let budget  = obj["thinking"]["budget_tokens"].as_i64().unwrap_or(0);
            let cur_max = obj.get("max_tokens").and_then(|v| v.as_i64()).unwrap_or(4096);
            if budget > 0 && cur_max <= budget {
                obj.insert("max_tokens".to_string(), json!(budget + 4096));
            }
        }
    }

    /// Converts OpenAI-format tool definitions to Anthropic format.
    /// OpenAI: { "type": "function", "function": { "name", "description", "parameters" } }
    /// Anthropic: { "name", "description", "input_schema" }
    fn convert_tools(tools: &[Value]) -> Vec<Value> {
        tools
            .iter()
            .filter_map(|t| {
                let func = &t["function"];
                let name = func["name"].as_str()?;
                Some(json!({
                    "name":         name,
                    "description":  func["description"].as_str().unwrap_or(""),
                    "input_schema": func["parameters"],
                }))
            })
            .collect()
    }

    /// Converts OpenAI-format message array to Anthropic format.
    ///
    /// Key differences:
    /// - System messages are skipped (extracted separately).
    /// - Assistant messages with `tool_calls` become content arrays with `tool_use` blocks.
    /// - `tool` role messages are grouped into `user` messages with `tool_result` blocks.
    fn convert_messages(messages: &[Value]) -> Vec<Value> {
        let mut out: Vec<Value> = Vec::new();
        let mut i = 0;

        while i < messages.len() {
            let msg  = &messages[i];
            let role = msg["role"].as_str().unwrap_or("");

            match role {
                "system" => { i += 1; }

                "user" => {
                    out.push(json!({
                        "role":    "user",
                        "content": convert_user_content(&msg["content"]),
                    }));
                    i += 1;
                }

                "assistant" => {
                    if let Some(tool_calls) = msg["tool_calls"].as_array() {
                        let mut content: Vec<Value> = Vec::new();

                        let text = msg["content"].as_str().unwrap_or("");
                        if !text.is_empty() {
                            content.push(json!({ "type": "text", "text": text }));
                        }

                        for tc in tool_calls {
                            let id       = tc["id"].as_str().unwrap_or("");
                            let name     = tc["function"]["name"].as_str().unwrap_or("");
                            let args_str = tc["function"]["arguments"].as_str().unwrap_or("{}");
                            let input: Value = serde_json::from_str(args_str)
                                .unwrap_or(Value::Object(Default::default()));

                            content.push(json!({
                                "type":  "tool_use",
                                "id":    id,
                                "name":  name,
                                "input": input,
                            }));
                        }

                        out.push(json!({ "role": "assistant", "content": content }));
                    } else {
                        out.push(json!({
                            "role":    "assistant",
                            "content": msg["content"].as_str().unwrap_or(""),
                        }));
                    }
                    i += 1;
                }

                "tool" => {
                    // Group all consecutive tool-result messages into a single user message.
                    let mut results: Vec<Value> = Vec::new();
                    while i < messages.len() && messages[i]["role"].as_str() == Some("tool") {
                        let tm = &messages[i];
                        results.push(json!({
                            "type":        "tool_result",
                            "tool_use_id": tm["tool_call_id"].as_str().unwrap_or(""),
                            "content":     tm["content"].as_str().unwrap_or(""),
                        }));
                        i += 1;
                    }
                    out.push(json!({ "role": "user", "content": results }));
                }

                _ => { i += 1; }
            }
        }

        out
    }
}

/// User content arrives either as a plain string or as an OpenAI-style parts
/// array (text + `image_url` data URLs, produced when the resolved model has
/// the `vision` capability). Strings pass through; parts become Anthropic
/// blocks. Video and unknown parts are dropped with a warning — providers
/// gate capabilities upstream, so this should only indicate a misconfigured
/// model row.
fn convert_user_content(content: &Value) -> Value {
    let Some(parts) = content.as_array() else {
        return Value::String(content.as_str().unwrap_or("").to_string());
    };
    let mut blocks = Vec::new();
    for p in parts {
        match p["type"].as_str().unwrap_or("") {
            "text" => blocks.push(json!({
                "type": "text",
                "text": p["text"].as_str().unwrap_or(""),
            })),
            "image_url" => {
                if let Some(block) = parse_data_image(&p["image_url"]) {
                    blocks.push(block);
                }
            }
            other => tracing::warn!(part_type = other, "dropping content part unsupported by Anthropic"),
        }
    }
    Value::Array(blocks)
}

/// `{"url": "data:<mime>;base64,<data>"}` (or the bare-string shorthand) → an
/// Anthropic base64 image block. Only data URLs are supported.
fn parse_data_image(image_url: &Value) -> Option<Value> {
    let url = image_url["url"].as_str().or_else(|| image_url.as_str())?;
    let (mime, data) = url.strip_prefix("data:")?.split_once(";base64,")?;
    Some(json!({
        "type": "image",
        "source": { "type": "base64", "media_type": mime, "data": data },
    }))
}

#[async_trait]
impl ChatbotClient for AnthropicClient {
    async fn chat(
        &self,
        messages: &[Message],
        options:  &ChatOptions,
    ) -> anyhow::Result<ChatResponse> {
        // Merge all system-role messages into a single `system:` parameter.
        let system: Option<String> = {
            let parts: Vec<&str> = messages
                .iter()
                .filter(|m| m.role == Role::System)
                .map(|m| m.content.as_str())
                .collect();
            if parts.is_empty() { None } else { Some(parts.join("\n\n---\n\n")) }
        };

        let msgs: Vec<Value> = messages
            .iter()
            .filter(|m| m.role != Role::System)
            .map(|m| {
                let role = match m.role {
                    Role::User      => "user",
                    Role::Assistant => "assistant",
                    Role::System    => unreachable!(),
                };
                json!({ "role": role, "content": m.content })
            })
            .collect();

        let max_tokens = options.max_tokens.unwrap_or(4096);
        let mut body = json!({
            "model":      options.model,
            "max_tokens": max_tokens,
            "messages":   msgs,
        });

        if let Some(sys) = system              { body["system"]      = sys.into(); }
        if let Some(t)   = options.temperature { body["temperature"] = t.into(); }
        self.apply_extra(&mut body);

        let url = format!("{}/v1/messages", self.base_url.trim_end_matches('/'));
        debug!(model = %options.model, "anthropic: sending chat request");
        trace!(body = %body, "anthropic: chat request body");

        let resp: Value = self
            .http
            .post(&url)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .json(&body)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;

        let content = resp["content"]
            .as_array()
            .and_then(|arr| arr.first())
            .and_then(|block| block["text"].as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing content in Anthropic response"))?
            .to_string();

        let input_tokens          = resp["usage"]["input_tokens"].as_u64().map(|n| n as u32);
        let output_tokens         = resp["usage"]["output_tokens"].as_u64().map(|n| n as u32);
        let cache_read_tokens     = resp["usage"]["cache_read_input_tokens"].as_u64().map(|n| n as u32);
        let cache_creation_tokens = resp["usage"]["cache_creation_input_tokens"].as_u64().map(|n| n as u32);
        info!(model = %options.model, ?input_tokens, ?output_tokens, "anthropic: chat response received");

        let cost = self.extract_cost(&resp);
        Ok(ChatResponse { content, input_tokens, output_tokens, truncated: false, reasoning_content: None, cache_read_tokens, cache_creation_tokens, cost })
    }

    async fn chat_with_tools(
        &self,
        messages: &[Value],
        tools:    &[Value],
        options:  &ChatOptions,
    ) -> anyhow::Result<LlmTurn> {
        self.chat_with_tools_raw(messages, tools, options).await.map(|(t, _)| t)
    }

    async fn chat_with_tools_raw(
        &self,
        messages: &[Value],
        tools:    &[Value],
        options:  &ChatOptions,
    ) -> anyhow::Result<(LlmTurn, Option<LlmRawMeta>)> {
        // Collect ALL system-role messages (main prompt, mid-conversation
        // summary, tail_reminder) and merge them into a single `system:`
        // string.  The Anthropic API only accepts a single system parameter;
        // mid-conversation system messages generated by build_openai_messages
        // are intentionally used for injecting compaction summaries and tail
        // reminders — they must not be silently dropped.
        let system: Option<String> = {
            let parts: Vec<&str> = messages
                .iter()
                .filter(|m| m["role"].as_str() == Some("system"))
                .filter_map(|m| m["content"].as_str())
                .collect();
            if parts.is_empty() { None } else { Some(parts.join("\n\n---\n\n")) }
        };

        let anthropic_messages = Self::convert_messages(messages);
        let anthropic_tools    = Self::convert_tools(tools);

        let max_tokens = options.max_tokens.unwrap_or(4096);
        let mut body = json!({
            "model":      options.model,
            "max_tokens": max_tokens,
            "messages":   anthropic_messages,
            "tools":      anthropic_tools,
        });

        if let Some(sys) = system              { body["system"]      = sys.into(); }
        if let Some(t)   = options.temperature { body["temperature"] = t.into(); }
        self.apply_extra(&mut body);

        let url = format!("{}/v1/messages", self.base_url.trim_end_matches('/'));
        debug!(model = %options.model, tools = tools.len(), "anthropic: sending chat_with_tools request");
        trace!(body = %body, "anthropic: chat_with_tools request body");

        // Capture request metadata for logging.
        let request_body    = body.clone();
        let request_headers = json!({
            "x-api-key":          redact_key(&self.api_key),
            "anthropic-version":  ANTHROPIC_VERSION,
            "content-type":       "application/json",
        });

        let http_resp = self
            .http
            .post(&url)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header("X-Title", core_api::APP_NAME)
            .json(&body)
            .send()
            .await?
            .error_for_status()?;

        let response_headers = headers_to_json(http_resp.headers());
        let resp_text        = http_resp.text().await?;
        let resp: Value      = serde_json::from_str(&resp_text)
            .map_err(|e| anyhow::anyhow!("anthropic: failed to parse response JSON: {e}\nbody: {resp_text}"))?;
        let response_body: Value = serde_json::from_str(&resp_text).unwrap_or(Value::Null);

        let raw_meta = LlmRawMeta {
            request_headers:  Some(request_headers),
            request_body:     Some(request_body),
            response_headers: Some(response_headers),
            response_body:    Some(response_body),
        };

        let stop_reason           = resp["stop_reason"].as_str().unwrap_or("");
        let input_tokens          = resp["usage"]["input_tokens"].as_u64().map(|n| n as u32);
        let output_tokens         = resp["usage"]["output_tokens"].as_u64().map(|n| n as u32);
        let cache_read_tokens     = resp["usage"]["cache_read_input_tokens"].as_u64().map(|n| n as u32);
        let cache_creation_tokens = resp["usage"]["cache_creation_input_tokens"].as_u64().map(|n| n as u32);
        let content_blocks        = resp["content"].as_array().cloned().unwrap_or_default();
        let cost                  = self.extract_cost(&resp);
        info!(model = %options.model, ?input_tokens, ?output_tokens, stop_reason, "anthropic: chat_with_tools response received");
        if stop_reason == "max_tokens" {
            warn!(model = %options.model, ?output_tokens, "anthropic: response truncated (max_tokens reached)");
        }

        let has_tool_use = content_blocks.iter().any(|b| b["type"].as_str() == Some("tool_use"));

        // Check content blocks directly: Anthropic sometimes returns stop_reason "end_turn"
        // even when tool_use blocks are present, so stop_reason alone is not reliable.
        let turn = if stop_reason == "tool_use" || has_tool_use {
            let text: String = content_blocks
                .iter()
                .filter(|b| b["type"].as_str() == Some("text"))
                .filter_map(|b| b["text"].as_str())
                .collect::<Vec<_>>()
                .join("\n");

            let calls: Vec<ToolCall> = content_blocks
                .iter()
                .filter(|b| b["type"].as_str() == Some("tool_use"))
                .map(|b| ToolCall {
                    id:        b["id"].as_str().unwrap_or("").to_string(),
                    name:      b["name"].as_str().unwrap_or("").to_string(),
                    arguments: b["input"].clone(),
                })
                .collect();

            LlmTurn::ToolCalls { content: text, calls, input_tokens, output_tokens, reasoning_content: None, cache_read_tokens, cache_creation_tokens, cost }
        } else {
            let content = content_blocks
                .iter()
                .find(|b| b["type"].as_str() == Some("text"))
                .and_then(|b| b["text"].as_str())
                .unwrap_or("")
                .to_string();

            let truncated = stop_reason == "max_tokens";
            LlmTurn::Message(ChatResponse { content, input_tokens, output_tokens, truncated, reasoning_content: None, cache_read_tokens, cache_creation_tokens, cost })
        };

        Ok((turn, Some(raw_meta)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_content_string_passthrough() {
        let v = convert_user_content(&json!("hello"));
        assert_eq!(v, json!("hello"));
    }

    #[test]
    fn user_content_parts_become_anthropic_blocks() {
        let v = convert_user_content(&json!([
            { "type": "text", "text": "what is this?" },
            { "type": "image_url", "image_url": { "url": "data:image/png;base64,QUJD" } },
        ]));
        assert_eq!(v, json!([
            { "type": "text", "text": "what is this?" },
            { "type": "image", "source": { "type": "base64", "media_type": "image/png", "data": "QUJD" } },
        ]));
    }

    #[test]
    fn user_content_drops_video_and_non_data_urls() {
        let v = convert_user_content(&json!([
            { "type": "text", "text": "t" },
            { "type": "video_url", "video_url": { "url": "data:video/mp4;base64,QUJD" } },
            { "type": "image_url", "image_url": { "url": "https://example.com/x.png" } },
        ]));
        assert_eq!(v, json!([{ "type": "text", "text": "t" }]));
    }
}
