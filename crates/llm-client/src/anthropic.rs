use std::collections::BTreeMap;

use async_trait::async_trait;
use futures_util::StreamExt;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tracing::{debug, info, trace, warn};

use crate::{ChatOptions, ChatResponse, ChatbotClient, LlmRawMeta, LlmTurn, Message, Role, SseDecoder, StreamDelta, ToolCall, error_response_body, headers_to_json, redact_key};

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
    ///
    /// DTL (tool search): a top-level `defer_loading: true` on the OpenAI tool
    /// object is carried through to Anthropic's native `defer_loading` field. When
    /// any tool is deferred, the cache breakpoint is placed on the last
    /// **non-deferred** tool — a deferred tool cannot also carry `cache_control`
    /// (the API 400s), and at least one tool must stay non-deferred anyway.
    fn convert_tools(tools: &[Value]) -> Vec<Value> {
        let has_deferred = tools.iter().any(|t| t["defer_loading"].as_bool() == Some(true));
        let mut out: Vec<Value> = tools
            .iter()
            .filter_map(|t| {
                let func = &t["function"];
                let name = func["name"].as_str()?;
                let mut tool = json!({
                    "name":         name,
                    "description":  func["description"].as_str().unwrap_or(""),
                    "input_schema": func["parameters"],
                });
                if t["defer_loading"].as_bool() == Some(true) {
                    tool["defer_loading"] = json!(true);
                }
                Some(tool)
            })
            .collect();
        if has_deferred {
            if let Some(t) = out.iter_mut().rev().find(|t| t["defer_loading"].as_bool() != Some(true)) {
                t["cache_control"] = json!({ "type": "ephemeral" });
            }
        }
        out
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
                        // DTL (custom tool search): a tool result carrying
                        // `_tool_references` (set by the message builder on an
                        // `activate_tools` result in AnthropicToolReference mode) becomes a
                        // `content` array of `tool_reference` blocks, which the API expands
                        // into the deferred tools' full definitions. Empty/absent → the
                        // normal text result.
                        let content: Value = match tm["_tool_references"].as_array() {
                            Some(refs) if !refs.is_empty() => Value::Array(
                                refs.iter()
                                    .filter_map(|r| r.as_str())
                                    .map(|name| json!({ "type": "tool_reference", "tool_name": name }))
                                    .collect(),
                            ),
                            _ => Value::String(tm["content"].as_str().unwrap_or("").to_string()),
                        };
                        results.push(json!({
                            "type":        "tool_result",
                            "tool_use_id": tm["tool_call_id"].as_str().unwrap_or(""),
                            "content":     content,
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

    /// Assembles the `/v1/messages` request body shared by the buffered and the
    /// streaming path (the caller adds `stream` on top).
    fn tools_body(&self, system: Option<Value>, messages: Vec<Value>, tools: Vec<Value>, options: &ChatOptions) -> Value {
        let max_tokens = options.max_tokens.unwrap_or(4096);
        let mut body = json!({
            "model":      options.model,
            "max_tokens": max_tokens,
            "messages":   messages,
            "tools":      tools,
        });

        if let Some(sys) = system              { body["system"]      = sys; }
        if let Some(t)   = options.temperature { body["temperature"] = t.into(); }
        self.apply_extra(&mut body);
        body
    }

    /// Collects ALL system-role messages (main prompt, mid-conversation summary,
    /// tail_reminder) into the single `system` parameter the Anthropic API accepts.
    ///
    /// Returns a plain string in the common case. When any system message carries
    /// **structured** content (a text-block array, e.g. the static prompt tagged
    /// with `cache_control` when prompt caching is on), it returns the array form
    /// instead so the cache breakpoint survives into `system`. String-content
    /// messages become plain text blocks (no cache_control).
    fn merged_system(messages: &[Value]) -> Option<Value> {
        let sys: Vec<&Value> = messages
            .iter()
            .filter(|m| m["role"].as_str() == Some("system"))
            .collect();
        if sys.is_empty() { return None; }

        if !sys.iter().any(|m| m["content"].is_array()) {
            let parts: Vec<&str> = sys.iter().filter_map(|m| m["content"].as_str()).collect();
            return if parts.is_empty() { None } else { Some(Value::String(parts.join("\n\n---\n\n"))) };
        }

        let mut blocks: Vec<Value> = Vec::new();
        for m in &sys {
            match &m["content"] {
                Value::String(s) if !s.is_empty() => blocks.push(json!({ "type": "text", "text": s })),
                Value::Array(arr) => {
                    for b in arr {
                        if b["type"].as_str() == Some("text") {
                            blocks.push(b.clone());
                        }
                    }
                }
                _ => {}
            }
        }
        if blocks.is_empty() { None } else { Some(Value::Array(blocks)) }
    }

    fn url(&self) -> String {
        format!("{}/v1/messages", self.base_url.trim_end_matches('/'))
    }

    fn logged_headers(&self) -> Value {
        json!({
            "x-api-key":          redact_key(&self.api_key),
            "anthropic-version":  ANTHROPIC_VERSION,
            "content-type":       "application/json",
        })
    }

    /// Sends the request and returns the raw response **without** `error_for_status`,
    /// so the tool-calling paths can read the error body and attach the request
    /// payload to the `LlmError` (a `reqwest` status error discards the body). The
    /// plain `chat` path keeps its own `error_for_status`.
    async fn send_request(&self, body: &Value) -> reqwest::Result<reqwest::Response> {
        self.http
            .post(self.url())
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header("X-Title", core_api::APP_NAME)
            .json(body)
            .send()
            .await
    }

    /// Joined `thinking` blocks of a content array, if any (extended thinking).
    fn reasoning_of(content_blocks: &[Value]) -> Option<String> {
        let parts: Vec<&str> = content_blocks
            .iter()
            .filter(|b| b["type"].as_str() == Some("thinking"))
            .filter_map(|b| b["thinking"].as_str())
            .collect();
        if parts.is_empty() { None } else { Some(parts.join("\n")) }
    }

    /// SSE streaming path behind `chat_with_tools_raw_streaming`. Anthropic
    /// streams typed events (`message_start` / `content_block_*` /
    /// `message_delta` / `message_stop`); text and thinking deltas are
    /// forwarded to `delta_tx` best-effort while the blocks are accumulated
    /// into the same `LlmTurn` the buffered path returns.
    async fn stream_chat(
        &self,
        messages: &[Value],
        tools:    &[Value],
        options:  &ChatOptions,
        delta_tx: &mpsc::Sender<StreamDelta>,
        emitted:  &mut bool,
    ) -> anyhow::Result<(LlmTurn, Option<LlmRawMeta>)> {
        let system            = Self::merged_system(messages);
        let anthropic_messages = Self::convert_messages(messages);
        let anthropic_tools    = Self::convert_tools(tools);
        let mut body = self.tools_body(system, anthropic_messages, anthropic_tools, options);
        body["stream"] = json!(true);

        debug!(model = %options.model, tools = tools.len(), "anthropic: sending streaming chat_with_tools request");
        trace!(body = %body, "anthropic: streaming chat_with_tools request body");

        let request_body    = body.clone();
        let request_headers = self.logged_headers();

        let http_resp        = self.send_request(&body).await?;
        let response_headers = headers_to_json(http_resp.headers());
        let status           = http_resp.status();
        if !status.is_success() {
            let resp_text = http_resp.text().await?;
            return Err(crate::LlmError {
                status:  Some(status.as_u16()),
                message: format!(
                    "anthropic: HTTP {status} from {url}\nbody: {resp_text}",
                    url = self.url(),
                ),
                raw_meta: Some(LlmRawMeta {
                    request_headers:  Some(request_headers),
                    request_body:     Some(request_body),
                    response_headers: Some(response_headers),
                    response_body:    Some(error_response_body(resp_text)),
                }),
            }.into());
        }

        /// One content block being accumulated by index.
        #[derive(Default)]
        struct Block {
            kind: String, // "text" | "thinking" | "tool_use"
            buf:  String, // text/thinking content or input_json fragments
            id:   String,
            name: String,
        }

        let mut blocks: BTreeMap<u64, Block> = BTreeMap::new();
        let mut stop_reason: Option<String> = None;
        let mut usage = json!({});
        let mut sse = SseDecoder::new();
        let mut byte_stream = http_resp.bytes_stream();

        let mut handle_payload = |payload: &str, emitted: &mut bool| -> anyhow::Result<()> {
            let Ok(v) = serde_json::from_str::<Value>(payload) else { return Ok(()) };
            match v["type"].as_str().unwrap_or("") {
                "message_start" => {
                    if let Some(u) = v["message"]["usage"].as_object() {
                        for (k, val) in u { usage[k.clone()] = val.clone(); }
                    }
                }
                "content_block_start" => {
                    let idx = v["index"].as_u64().unwrap_or(0);
                    let cb  = &v["content_block"];
                    let block = blocks.entry(idx).or_default();
                    block.kind = cb["type"].as_str().unwrap_or("").to_string();
                    block.id   = cb["id"].as_str().unwrap_or("").to_string();
                    block.name = cb["name"].as_str().unwrap_or("").to_string();
                }
                "content_block_delta" => {
                    let idx   = v["index"].as_u64().unwrap_or(0);
                    let delta = &v["delta"];
                    match delta["type"].as_str().unwrap_or("") {
                        "text_delta" => {
                            if let Some(t) = delta["text"].as_str().filter(|t| !t.is_empty()) {
                                blocks.entry(idx).or_default().buf.push_str(t);
                                *emitted = true;
                                let _ = delta_tx.try_send(StreamDelta::Text(t.to_string()));
                            }
                        }
                        "thinking_delta" => {
                            if let Some(t) = delta["thinking"].as_str().filter(|t| !t.is_empty()) {
                                blocks.entry(idx).or_default().buf.push_str(t);
                                *emitted = true;
                                let _ = delta_tx.try_send(StreamDelta::Reasoning(t.to_string()));
                            }
                        }
                        "input_json_delta" => {
                            if let Some(j) = delta["partial_json"].as_str() {
                                blocks.entry(idx).or_default().buf.push_str(j);
                            }
                        }
                        // signature_delta and unknown deltas carry no displayable text.
                        _ => {}
                    }
                }
                "message_delta" => {
                    if let Some(sr) = v["delta"]["stop_reason"].as_str() {
                        stop_reason = Some(sr.to_string());
                    }
                    if let Some(u) = v["usage"].as_object() {
                        for (k, val) in u { usage[k.clone()] = val.clone(); }
                    }
                }
                "error" => {
                    return Err(anyhow::anyhow!("anthropic: stream error event: {payload}"));
                }
                // content_block_stop / message_stop / ping: nothing to accumulate.
                _ => {}
            }
            Ok(())
        };

        while let Some(chunk) = byte_stream.next().await {
            let chunk = chunk?;
            for payload in sse.feed(&chunk) {
                handle_payload(&payload, emitted)?;
            }
        }
        for payload in sse.finish() {
            handle_payload(&payload, emitted)?;
        }

        let stop                  = stop_reason.as_deref().unwrap_or("");
        let input_tokens          = usage["input_tokens"].as_u64().map(|n| n as u32);
        let output_tokens         = usage["output_tokens"].as_u64().map(|n| n as u32);
        let cache_read_tokens     = usage["cache_read_input_tokens"].as_u64().map(|n| n as u32);
        let cache_creation_tokens = usage["cache_creation_input_tokens"].as_u64().map(|n| n as u32);
        info!(model = %options.model, ?input_tokens, ?output_tokens, stop_reason = stop, "anthropic: streaming response completed");
        if stop == "max_tokens" {
            warn!(model = %options.model, ?output_tokens, "anthropic: response truncated (max_tokens reached)");
        }

        let text_of = |kind: &str| -> String {
            blocks.values()
                .filter(|b| b.kind == kind)
                .map(|b| b.buf.as_str())
                .collect::<Vec<_>>()
                .join("\n")
        };
        let reasoning = text_of("thinking");
        let reasoning_content = if reasoning.is_empty() { None } else { Some(reasoning) };
        let tool_blocks: Vec<&Block> = blocks.values().filter(|b| b.kind == "tool_use").collect();

        let turn = if !tool_blocks.is_empty() {
            let calls = tool_blocks
                .iter()
                .map(|b| ToolCall {
                    id:        b.id.clone(),
                    name:      b.name.clone(),
                    arguments: serde_json::from_str(&b.buf).unwrap_or(Value::Object(Default::default())),
                })
                .collect();
            LlmTurn::ToolCalls { content: text_of("text"), calls, input_tokens, output_tokens, reasoning_content, cache_read_tokens, cache_creation_tokens, cost: None }
        } else {
            let truncated = stop == "max_tokens";
            LlmTurn::Message(ChatResponse {
                content: text_of("text"), input_tokens, output_tokens, truncated,
                reasoning_content, cache_read_tokens, cache_creation_tokens, cost: None,
            })
        };

        // Buffered-shaped response body for the payload log.
        let content_log: Vec<Value> = blocks.values().map(|b| match b.kind.as_str() {
            "tool_use"  => json!({"type": "tool_use", "id": b.id, "name": b.name, "input": serde_json::from_str::<Value>(&b.buf).unwrap_or(json!({}))}),
            "thinking"  => json!({"type": "thinking", "thinking": b.buf}),
            _           => json!({"type": "text", "text": b.buf}),
        }).collect();
        let raw_meta = LlmRawMeta {
            request_headers:  Some(request_headers),
            request_body:     Some(request_body),
            response_headers: Some(response_headers),
            response_body:    Some(json!({
                "streamed": true,
                "content": content_log,
                "stop_reason": stop,
                "usage": usage,
            })),
        };

        Ok((turn, Some(raw_meta)))
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
            "file" => {
                if let Some(block) = parse_data_document(&p["file"]) {
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

/// `{"file_data": "data:application/pdf;base64,<data>"}` → an Anthropic base64
/// `document` block (the native PDF input). Only base64 data URLs are supported;
/// the OpenAI `file` part is what the media pipeline emits for a PDF.
fn parse_data_document(file: &Value) -> Option<Value> {
    let url = file["file_data"].as_str()?;
    let (mime, data) = url.strip_prefix("data:")?.split_once(";base64,")?;
    Some(json!({
        "type": "document",
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
            .and_then(|arr| arr.iter().find(|b| b["type"].as_str() == Some("text")))
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
        // Mid-conversation system messages (compaction summaries, tail
        // reminders) are merged into the single `system:` parameter — they
        // must not be silently dropped.
        let system             = Self::merged_system(messages);
        let anthropic_messages = Self::convert_messages(messages);
        let anthropic_tools    = Self::convert_tools(tools);
        let body = self.tools_body(system, anthropic_messages, anthropic_tools, options);

        debug!(model = %options.model, tools = tools.len(), "anthropic: sending chat_with_tools request");
        trace!(body = %body, "anthropic: chat_with_tools request body");

        // Capture request metadata for logging.
        let request_body    = body.clone();
        let request_headers = self.logged_headers();

        let http_resp = self.send_request(&body).await?;

        let response_headers = headers_to_json(http_resp.headers());
        let status           = http_resp.status();
        let resp_text        = http_resp.text().await?;
        if !status.is_success() {
            return Err(crate::LlmError {
                status:  Some(status.as_u16()),
                message: format!(
                    "anthropic: HTTP {status} from {url}\nbody: {resp_text}",
                    url = self.url(),
                ),
                raw_meta: Some(LlmRawMeta {
                    request_headers:  Some(request_headers),
                    request_body:     Some(request_body),
                    response_headers: Some(response_headers),
                    response_body:    Some(error_response_body(resp_text)),
                }),
            }.into());
        }
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
        let reasoning_content = Self::reasoning_of(&content_blocks);

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

            LlmTurn::ToolCalls { content: text, calls, input_tokens, output_tokens, reasoning_content, cache_read_tokens, cache_creation_tokens, cost }
        } else {
            let content = content_blocks
                .iter()
                .find(|b| b["type"].as_str() == Some("text"))
                .and_then(|b| b["text"].as_str())
                .unwrap_or("")
                .to_string();

            let truncated = stop_reason == "max_tokens";
            LlmTurn::Message(ChatResponse { content, input_tokens, output_tokens, truncated, reasoning_content, cache_read_tokens, cache_creation_tokens, cost })
        };

        Ok((turn, Some(raw_meta)))
    }

    async fn chat_with_tools_raw_streaming(
        &self,
        messages: &[Value],
        tools:    &[Value],
        options:  &ChatOptions,
        delta_tx: mpsc::Sender<StreamDelta>,
    ) -> anyhow::Result<(LlmTurn, Option<LlmRawMeta>)> {
        let mut emitted = false;
        match self.stream_chat(messages, tools, options, &delta_tx, &mut emitted).await {
            Ok(ok) => Ok(ok),
            // Pre-stream failure (nothing shown yet): retry buffered. A
            // mid-stream failure propagates to the model-fallback logic.
            Err(e) if !emitted => {
                debug!(model = %options.model, error = %e, "anthropic: streaming failed before any delta; retrying buffered");
                self.chat_with_tools_raw(messages, tools, options).await
            }
            Err(e) => Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reasoning_of_joins_thinking_blocks() {
        let blocks = vec![
            json!({"type": "thinking", "thinking": "first"}),
            json!({"type": "text", "text": "answer"}),
            json!({"type": "thinking", "thinking": "second"}),
        ];
        assert_eq!(
            AnthropicClient::reasoning_of(&blocks),
            Some("first\nsecond".to_string())
        );
        assert_eq!(AnthropicClient::reasoning_of(&[]), None);
        assert_eq!(
            AnthropicClient::reasoning_of(&[json!({"type": "text", "text": "a"})]),
            None
        );
    }

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

    #[test]
    fn user_content_file_part_becomes_document_block() {
        // The OpenAI `file` part (emitted by the media pipeline for a PDF) becomes
        // an Anthropic native `document` block.
        let v = convert_user_content(&json!([
            { "type": "text", "text": "read this" },
            { "type": "file", "file": { "filename": "a.pdf", "file_data": "data:application/pdf;base64,QUJD" } },
        ]));
        assert_eq!(v, json!([
            { "type": "text", "text": "read this" },
            { "type": "document", "source": { "type": "base64", "media_type": "application/pdf", "data": "QUJD" } },
        ]));

        // A non-data file_data (or missing) is dropped, not forwarded.
        let v = convert_user_content(&json!([
            { "type": "file", "file": { "filename": "a.pdf", "file_data": "https://example.com/a.pdf" } },
        ]));
        assert_eq!(v, json!([]));
    }
}
