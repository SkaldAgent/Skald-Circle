//! Anthropic client (`/v1/messages`). Ported from `llm-client/src/anthropic.rs`
//! onto the `Model` trait — including the DTL conversions (blueprint §4.10):
//! `defer_loading`, `_tool_references` → `tool_reference` blocks, and the
//! `cache_control` breakpoint moved onto the last non-deferred tool.

use std::collections::BTreeMap;

use async_trait::async_trait;
use futures_util::StreamExt;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tracing::{debug, info, trace, warn};

use super::{SseDecoder, error_response_body, headers_to_json, redact_key};
use crate::APP_NAME;
use crate::model::{
    Model, ModelError, ModelRequest, ModelResponse, NamedModel, RawMeta, StreamDelta, ToolCall,
    Usage,
};

const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
const ANTHROPIC_VERSION: &str = "2023-06-01";

pub struct AnthropicModel {
    base_url:      String,
    api_key:       String,
    default_model: String,
    /// Extra top-level request-body keys merged into every request (e.g. the
    /// `thinking` config for extended reasoning).
    extra_body:    Option<Value>,
    app_name:      String,
    http:          reqwest::Client,
}

impl AnthropicModel {
    pub fn new(api_key: impl Into<String>, default_model: impl Into<String>) -> Self {
        Self::with_extra_body(api_key, default_model, None)
    }

    pub fn with_base_url(
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        default_model: impl Into<String>,
    ) -> Self {
        Self {
            base_url: base_url.into(),
            api_key: api_key.into(),
            default_model: default_model.into(),
            extra_body: None,
            app_name: APP_NAME.to_string(),
            http: reqwest::Client::new(),
        }
    }

    /// Like `new` but with extra request-body keys (e.g. `{"thinking": {...}}`).
    pub fn with_extra_body(
        api_key: impl Into<String>,
        default_model: impl Into<String>,
        extra_body: Option<Value>,
    ) -> Self {
        Self {
            base_url: DEFAULT_BASE_URL.to_string(),
            api_key: api_key.into(),
            default_model: default_model.into(),
            extra_body,
            app_name: APP_NAME.to_string(),
            http: reqwest::Client::new(),
        }
    }

    pub fn with_app_name(mut self, app_name: impl Into<String>) -> Self {
        self.app_name = app_name.into();
        self
    }

    /// Merges `extra_body` (then the request's own `extras`) into `body` and
    /// enforces Anthropic's extended-thinking constraints: when `thinking` is
    /// enabled, `temperature` is not allowed and `max_tokens` must be strictly
    /// greater than `budget_tokens`.
    fn apply_extra(&self, body: &mut Value, req_extras: &Value) {
        for extra in [self.extra_body.as_ref(), Some(req_extras).filter(|v| v.is_object())]
            .into_iter()
            .flatten()
        {
            let Some(extra) = extra.as_object() else { continue };
            let Some(obj) = body.as_object_mut() else { return };
            for (k, v) in extra {
                obj.insert(k.clone(), v.clone());
            }
        }
        let Some(obj) = body.as_object_mut() else { return };
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
    /// DTL (`DeferredToolReference`): a top-level `defer_loading: true` on the
    /// OpenAI tool object is carried through. When any tool is deferred, the
    /// cache breakpoint is placed on the last **non-deferred** tool — a
    /// deferred tool cannot carry `cache_control` (the API 400s).
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
        if has_deferred
            && let Some(t) = out.iter_mut().rev().find(|t| t["defer_loading"].as_bool() != Some(true))
        {
            t["cache_control"] = json!({ "type": "ephemeral" });
        }
        out
    }

    /// Converts OpenAI-format messages to Anthropic format: system extracted
    /// separately; assistant tool_calls → tool_use blocks; consecutive `tool`
    /// messages grouped into one user message of tool_result blocks.
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
                    // Group consecutive tool results into a single user message.
                    let mut results: Vec<Value> = Vec::new();
                    while i < messages.len() && messages[i]["role"].as_str() == Some("tool") {
                        let tm = &messages[i];
                        // DTL (`DeferredToolReference`): a tool result carrying
                        // `_tool_references` becomes a content array of
                        // `tool_reference` blocks, which the API expands into
                        // the deferred tools' full definitions.
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

    /// Shared `/v1/messages` body (the caller adds `stream` on top).
    fn tools_body(&self, system: Option<Value>, messages: Vec<Value>, tools: Vec<Value>, req: &ModelRequest) -> Value {
        let max_tokens = req.max_tokens.unwrap_or(4096);
        let mut body = json!({
            "model":      req.model,
            "max_tokens": max_tokens,
            "messages":   messages,
            "tools":      tools,
        });

        if let Some(sys) = system          { body["system"]      = sys; }
        if let Some(t)   = req.temperature { body["temperature"] = t.into(); }
        self.apply_extra(&mut body, &req.extras);
        body
    }

    /// Collects ALL system-role messages into the single `system` parameter.
    /// Structured content (a text-block array with `cache_control`) is kept
    /// in array form so the cache breakpoint survives.
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

    /// Sends the request WITHOUT `error_for_status`, so the caller can read
    /// the error body and attach the payload to the `ModelError`.
    async fn send_request(&self, body: &Value) -> Result<reqwest::Response, ModelError> {
        self.http
            .post(self.url())
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header("X-Title", &self.app_name)
            .json(body)
            .send()
            .await
            .map_err(ModelError::from_reqwest)
    }

    /// Joined `thinking` blocks of a content array (extended thinking).
    fn reasoning_of(content_blocks: &[Value]) -> Option<String> {
        let parts: Vec<&str> = content_blocks
            .iter()
            .filter(|b| b["type"].as_str() == Some("thinking"))
            .filter_map(|b| b["thinking"].as_str())
            .collect();
        if parts.is_empty() { None } else { Some(parts.join("\n")) }
    }

    /// The buffered path.
    async fn buffered(&self, req: &ModelRequest) -> Result<ModelResponse, ModelError> {
        let system             = Self::merged_system(&req.messages);
        let anthropic_messages = Self::convert_messages(&req.messages);
        let anthropic_tools    = Self::convert_tools(&req.tools);
        let body = self.tools_body(system, anthropic_messages, anthropic_tools, req);

        debug!(model = %req.model, tools = req.tools.len(), "anthropic: sending request");
        trace!(body = %body, "anthropic: request body");

        let request_body    = body.clone();
        let request_headers = self.logged_headers();

        let http_resp = self.send_request(&body).await?;

        let response_headers = headers_to_json(http_resp.headers());
        let status           = http_resp.status();
        let resp_text        = http_resp.text().await.map_err(ModelError::from_reqwest)?;
        if !status.is_success() {
            return Err(ModelError {
                status:  Some(status.as_u16()),
                message: format!("anthropic: HTTP {status} from {url}\nbody: {resp_text}", url = self.url()),
                raw:     Some(RawMeta {
                    request_headers:  Some(request_headers),
                    request_body:     Some(request_body),
                    response_headers: Some(response_headers),
                    response_body:    Some(error_response_body(resp_text)),
                }),
            });
        }
        let resp: Value = serde_json::from_str(&resp_text).map_err(|e| {
            ModelError::new(None, format!("anthropic: failed to parse response JSON: {e}\nbody: {resp_text}"))
        })?;

        let raw = RawMeta {
            request_headers:  Some(request_headers),
            request_body:     Some(request_body),
            response_headers: Some(response_headers),
            response_body:    Some(resp.clone()),
        };

        let stop_reason = resp["stop_reason"].as_str().unwrap_or("");
        let mut usage = Usage {
            input_tokens:  resp["usage"]["input_tokens"].as_u64().map(|n| n as u32),
            output_tokens: resp["usage"]["output_tokens"].as_u64().map(|n| n as u32),
            cache_read:    resp["usage"]["cache_read_input_tokens"].as_u64().map(|n| n as u32),
            cache_write:   resp["usage"]["cache_creation_input_tokens"].as_u64().map(|n| n as u32),
            cost_usd:      None,
            truncated:     stop_reason == "max_tokens",
        };
        let content_blocks = resp["content"].as_array().cloned().unwrap_or_default();
        info!(model = %req.model, ?usage.input_tokens, ?usage.output_tokens, stop_reason, "anthropic: response received");
        if usage.truncated {
            warn!(model = %req.model, ?usage.output_tokens, "anthropic: response truncated (max_tokens reached)");
        }

        let has_tool_use = content_blocks.iter().any(|b| b["type"].as_str() == Some("tool_use"));
        let reasoning = Self::reasoning_of(&content_blocks);

        // Anthropic sometimes returns stop_reason "end_turn" even when
        // tool_use blocks are present — check the blocks directly.
        let mut resp_out = if stop_reason == "tool_use" || has_tool_use {
            let text: String = content_blocks
                .iter()
                .filter(|b| b["type"].as_str() == Some("text"))
                .filter_map(|b| b["text"].as_str())
                .collect::<Vec<_>>()
                .join("\n");
            usage.truncated = false;
            let calls: Vec<ToolCall> = content_blocks
                .iter()
                .filter(|b| b["type"].as_str() == Some("tool_use"))
                .map(|b| ToolCall {
                    id:        b["id"].as_str().unwrap_or("").to_string(),
                    name:      b["name"].as_str().unwrap_or("").to_string(),
                    arguments: b["input"].clone(),
                })
                .collect();
            ModelResponse::ToolCalls { content: text, calls, reasoning, usage, raw: None }
        } else {
            let content = content_blocks
                .iter()
                .find(|b| b["type"].as_str() == Some("text"))
                .and_then(|b| b["text"].as_str())
                .unwrap_or("")
                .to_string();
            ModelResponse::Message { content, reasoning, usage, raw: None }
        };
        match &mut resp_out {
            ModelResponse::Message { raw: r, .. } | ModelResponse::ToolCalls { raw: r, .. } => {
                *r = Some(raw)
            }
        }
        Ok(resp_out)
    }

    /// SSE streaming path: Anthropic streams typed events (`message_start` /
    /// `content_block_*` / `message_delta`); text and thinking deltas are
    /// forwarded best-effort while blocks accumulate into the same
    /// `ModelResponse` the buffered path returns.
    #[allow(clippy::result_large_err)]
    async fn stream_chat(
        &self,
        req:      &ModelRequest,
        delta_tx: &mpsc::Sender<StreamDelta>,
        emitted:  &mut bool,
    ) -> Result<ModelResponse, ModelError> {
        let system             = Self::merged_system(&req.messages);
        let anthropic_messages = Self::convert_messages(&req.messages);
        let anthropic_tools    = Self::convert_tools(&req.tools);
        let mut body = self.tools_body(system, anthropic_messages, anthropic_tools, req);
        body["stream"] = json!(true);

        debug!(model = %req.model, tools = req.tools.len(), "anthropic: sending streaming request");
        trace!(body = %body, "anthropic: streaming request body");

        let request_body    = body.clone();
        let request_headers = self.logged_headers();

        let http_resp        = self.send_request(&body).await?;
        let response_headers = headers_to_json(http_resp.headers());
        let status           = http_resp.status();
        if !status.is_success() {
            let resp_text = http_resp.text().await.map_err(ModelError::from_reqwest)?;
            return Err(ModelError {
                status:  Some(status.as_u16()),
                message: format!("anthropic: HTTP {status} from {url}\nbody: {resp_text}", url = self.url()),
                raw:     Some(RawMeta {
                    request_headers:  Some(request_headers),
                    request_body:     Some(request_body),
                    response_headers: Some(response_headers),
                    response_body:    Some(error_response_body(resp_text)),
                }),
            });
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

        let mut handle_payload = |payload: &str, emitted: &mut bool| -> Result<(), ModelError> {
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
                    return Err(ModelError::new(None, format!("anthropic: stream error event: {payload}")));
                }
                _ => {}
            }
            Ok(())
        };

        while let Some(chunk) = byte_stream.next().await {
            let chunk = chunk.map_err(ModelError::from_reqwest)?;
            for payload in sse.feed(&chunk) {
                handle_payload(&payload, emitted)?;
            }
        }
        for payload in sse.finish() {
            handle_payload(&payload, emitted)?;
        }

        let stop = stop_reason.as_deref().unwrap_or("");
        let usage_struct = Usage {
            input_tokens:  usage["input_tokens"].as_u64().map(|n| n as u32),
            output_tokens: usage["output_tokens"].as_u64().map(|n| n as u32),
            cache_read:    usage["cache_read_input_tokens"].as_u64().map(|n| n as u32),
            cache_write:   usage["cache_creation_input_tokens"].as_u64().map(|n| n as u32),
            cost_usd:      None,
            truncated:     stop == "max_tokens",
        };
        info!(model = %req.model, ?usage_struct.input_tokens, ?usage_struct.output_tokens, stop_reason = stop, "anthropic: streaming response completed");
        if usage_struct.truncated {
            warn!(model = %req.model, "anthropic: response truncated (max_tokens reached)");
        }

        let text_of = |kind: &str| -> String {
            blocks.values()
                .filter(|b| b.kind == kind)
                .map(|b| b.buf.as_str())
                .collect::<Vec<_>>()
                .join("\n")
        };
        let reasoning_text = text_of("thinking");
        let reasoning = if reasoning_text.is_empty() { None } else { Some(reasoning_text) };
        let tool_blocks: Vec<&Block> = blocks.values().filter(|b| b.kind == "tool_use").collect();

        // Buffered-shaped response body for the payload log.
        let content_log: Vec<Value> = blocks.values().map(|b| match b.kind.as_str() {
            "tool_use"  => json!({"type": "tool_use", "id": b.id, "name": b.name, "input": serde_json::from_str::<Value>(&b.buf).unwrap_or(json!({}))}),
            "thinking"  => json!({"type": "thinking", "thinking": b.buf}),
            _           => json!({"type": "text", "text": b.buf}),
        }).collect();
        let raw = RawMeta {
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

        let mut resp_out = if !tool_blocks.is_empty() {
            let calls = tool_blocks
                .iter()
                .map(|b| ToolCall {
                    id:        b.id.clone(),
                    name:      b.name.clone(),
                    arguments: serde_json::from_str(&b.buf).unwrap_or(Value::Object(Default::default())),
                })
                .collect();
            ModelResponse::ToolCalls { content: text_of("text"), calls, reasoning, usage: usage_struct, raw: None }
        } else {
            ModelResponse::Message { content: text_of("text"), reasoning, usage: usage_struct, raw: None }
        };
        match &mut resp_out {
            ModelResponse::Message { raw: r, .. } | ModelResponse::ToolCalls { raw: r, .. } => {
                *r = Some(raw)
            }
        }
        Ok(resp_out)
    }
}

impl NamedModel for AnthropicModel {
    fn default_model(&self) -> &str { &self.default_model }
}

#[async_trait]
impl Model for AnthropicModel {
    async fn complete(
        &self,
        req:    &ModelRequest,
        deltas: Option<mpsc::Sender<StreamDelta>>,
    ) -> Result<ModelResponse, ModelError> {
        match deltas {
            None => self.buffered(req).await,
            Some(delta_tx) => {
                let mut emitted = false;
                match self.stream_chat(req, &delta_tx, &mut emitted).await {
                    Ok(ok) => Ok(ok),
                    // Pre-stream failure (nothing shown yet): retry buffered.
                    // A mid-stream failure propagates to the fallback logic.
                    Err(e) if !emitted => {
                        debug!(model = %req.model, error = %e, "anthropic: streaming failed before any delta; retrying buffered");
                        self.buffered(req).await
                    }
                    Err(e) => Err(e),
                }
            }
        }
    }
}

/// User content arrives either as a plain string or as an OpenAI-style parts
/// array (text + `image_url` data URLs + `file` PDF parts). Strings pass
/// through; parts become Anthropic blocks. Unknown parts are dropped with a
/// warning.
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

/// `{"url": "data:<mime>;base64,<data>"}` → an Anthropic base64 image block.
fn parse_data_image(image_url: &Value) -> Option<Value> {
    let url = image_url["url"].as_str().or_else(|| image_url.as_str())?;
    let (mime, data) = url.strip_prefix("data:")?.split_once(";base64,")?;
    Some(json!({
        "type": "image",
        "source": { "type": "base64", "media_type": mime, "data": data },
    }))
}

/// `{"file_data": "data:application/pdf;base64,<data>"}` → an Anthropic
/// base64 `document` block (the native PDF input).
fn parse_data_document(file: &Value) -> Option<Value> {
    let url = file["file_data"].as_str()?;
    let (mime, data) = url.strip_prefix("data:")?.split_once(";base64,")?;
    Some(json!({
        "type": "document",
        "source": { "type": "base64", "media_type": mime, "data": data },
    }))
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
            AnthropicModel::reasoning_of(&blocks),
            Some("first\nsecond".to_string())
        );
        assert_eq!(AnthropicModel::reasoning_of(&[]), None);
        assert_eq!(
            AnthropicModel::reasoning_of(&[json!({"type": "text", "text": "a"})]),
            None
        );
    }

    #[test]
    fn convert_tools_carries_defer_loading_and_moves_cache_control() {
        let tools = vec![
            json!({"type":"function","function":{"name":"a","description":"","parameters":{}}}),
            json!({"type":"function","function":{"name":"b","description":"","parameters":{}},"defer_loading":true}),
            json!({"type":"function","function":{"name":"c","description":"","parameters":{}},"defer_loading":true}),
        ];
        let out = AnthropicModel::convert_tools(&tools);
        assert_eq!(out[0]["cache_control"], json!({"type": "ephemeral"}));
        assert!(out[0].get("defer_loading").is_none());
        assert_eq!(out[1]["defer_loading"], json!(true));
        assert!(out[1].get("cache_control").is_none());
        assert_eq!(out[2]["defer_loading"], json!(true));
    }

    #[test]
    fn convert_messages_tool_references_become_blocks() {
        let messages = vec![
            json!({"role":"assistant","content":"","tool_calls":[
                {"id":"t1","type":"function","function":{"name":"activate_tools","arguments":"{\"groups\":[\"gmail\"]}"}}
            ]}),
            json!({"role":"tool","tool_call_id":"t1","content":"ok","_tool_references":["mcp__gmail__send"]}),
        ];
        let out = AnthropicModel::convert_messages(&messages);
        assert_eq!(out.len(), 2);
        let results = out[1]["content"].as_array().unwrap();
        assert_eq!(
            results[0]["content"],
            json!([{ "type": "tool_reference", "tool_name": "mcp__gmail__send" }])
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
        let v = convert_user_content(&json!([
            { "type": "text", "text": "read this" },
            { "type": "file", "file": { "filename": "a.pdf", "file_data": "data:application/pdf;base64,QUJD" } },
        ]));
        assert_eq!(v, json!([
            { "type": "text", "text": "read this" },
            { "type": "document", "source": { "type": "base64", "media_type": "application/pdf", "data": "QUJD" } },
        ]));

        let v = convert_user_content(&json!([
            { "type": "file", "file": { "filename": "a.pdf", "file_data": "https://example.com/a.pdf" } },
        ]));
        assert_eq!(v, json!([]));
    }
}
