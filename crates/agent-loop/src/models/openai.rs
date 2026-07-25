//! OpenAI-compatible client (OpenAI, OpenRouter, Moonshot/Kimi, and every
//! provider declared via YAML). Ported from `llm-client/src/openai.rs` onto
//! the `Model` trait.
//!
//! Kimi's `SystemToolBlock` DTL needs NO client code: messages are passed
//! through verbatim and the endpoint speaks the `{role:"system", tools:[…]}`
//! convention natively.

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

/// OpenAI ChatGPT client (also compatible with any OpenAI-spec endpoint).
pub struct OpenAiModel {
    base_url:            String,
    api_key:             String,
    default_model:       String,
    extra_params:        Option<Value>,
    /// When true, Anthropic-compatible prompt-caching hints are injected
    /// (OpenRouter routing to Anthropic models).
    enable_prompt_cache: bool,
    app_name:            String,
    http:                reqwest::Client,
}

impl OpenAiModel {
    /// Minimal constructor: base URL + key + default model name (used as the
    /// selector id by `SingleModel`).
    pub fn new(
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        default_model: impl Into<String>,
    ) -> Self {
        Self::with_options(base_url, api_key, default_model, None, false)
    }

    pub fn with_options(
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        default_model: impl Into<String>,
        extra_params: Option<Value>,
        enable_prompt_cache: bool,
    ) -> Self {
        Self {
            base_url: base_url.into(),
            api_key: api_key.into(),
            default_model: default_model.into(),
            extra_params,
            enable_prompt_cache,
            app_name: APP_NAME.to_string(),
            http: reqwest::Client::new(),
        }
    }

    /// Override the `X-Title` header (OpenRouter rankings).
    pub fn with_app_name(mut self, app_name: impl Into<String>) -> Self {
        self.app_name = app_name.into();
        self
    }

    /// Merges extra top-level object keys into `body` (later maps win).
    fn merge_extra(body: &mut Value, extra: Option<&Value>) {
        if let Some(Value::Object(extra)) = extra
            && let Some(b) = body.as_object_mut()
        {
            for (k, v) in extra {
                b.insert(k.clone(), v.clone());
            }
        }
    }

    fn url(&self) -> String {
        format!("{}/chat/completions", self.base_url.trim_end_matches('/'))
    }

    /// Shared request body for the buffered and the streaming path.
    fn base_body(&self, model: &str, messages: &[Value], tools: &[Value]) -> Value {
        let mut body = json!({
            "model":    model,
            "messages": messages,
        });

        if !tools.is_empty() {
            // When prompt caching is enabled, tag the last tool with cache_control
            // so the entire tools array is included in the KV cache prefix.
            let tools_value: Value = if self.enable_prompt_cache {
                let mut tagged = tools.to_vec();
                if let Some(last) = tagged.last_mut() {
                    last["cache_control"] = json!({"type": "ephemeral"});
                }
                tagged.into()
            } else {
                tools.into()
            };
            body["tools"]       = tools_value;
            body["tool_choice"] = "auto".into();
        }
        body
    }

    fn finalize_body(&self, mut body: Value, req: &ModelRequest) -> Value {
        if let Some(t) = req.max_tokens  { body["max_tokens"]  = t.into(); }
        if let Some(t) = req.temperature { body["temperature"] = t.into(); }
        Self::merge_extra(&mut body, self.extra_params.as_ref());
        Self::merge_extra(&mut body, Some(&req.extras));
        body
    }

    /// Request metadata for logging (shared by buffered and streaming paths).
    fn logged_headers(&self) -> Value {
        let mut logged_headers = json!({
            "authorization": format!("Bearer {}", redact_key(&self.api_key)),
            "content-type":  "application/json",
        });
        if self.enable_prompt_cache {
            logged_headers["anthropic-beta"] = "prompt-caching-2024-07-31".into();
        }
        logged_headers
    }

    async fn send_request(&self, body: &Value) -> Result<reqwest::Response, ModelError> {
        let mut req = self
            .http
            .post(self.url())
            .bearer_auth(&self.api_key)
            .header("X-Title", &self.app_name);
        if self.enable_prompt_cache {
            req = req.header("anthropic-beta", "prompt-caching-2024-07-31");
        }
        req.json(body).send().await.map_err(ModelError::from_reqwest)
    }

    /// The buffered path.
    async fn buffered(&self, req: &ModelRequest) -> Result<ModelResponse, ModelError> {
        let body = self.finalize_body(self.base_body(&req.model, &req.messages, &req.tools), req);

        debug!(model = %req.model, tools = req.tools.len(), prompt_cache = self.enable_prompt_cache, "openai: sending request");
        trace!(body = %body, "openai: request body");

        let request_body    = body.clone();
        let request_headers = self.logged_headers();

        let http_resp = self.send_request(&body).await?;

        let response_headers = headers_to_json(http_resp.headers());
        let status           = http_resp.status();
        let resp_text        = http_resp.text().await.map_err(ModelError::from_reqwest)?;

        if !status.is_success() {
            return Err(ModelError {
                status:  Some(status.as_u16()),
                message: format!("openai: HTTP {status} from {url}\nbody: {resp_text}", url = self.url()),
                raw:     Some(RawMeta {
                    request_headers:  Some(request_headers),
                    request_body:     Some(request_body),
                    response_headers: Some(response_headers),
                    response_body:    Some(error_response_body(resp_text)),
                }),
            });
        }

        let resp: Value = serde_json::from_str(&resp_text).map_err(|e| {
            ModelError::new(None, format!("openai: failed to parse response JSON: {e}\nbody: {resp_text}"))
        })?;
        let response_body: Value = serde_json::from_str(&resp_text).unwrap_or(Value::Null);

        let raw = RawMeta {
            request_headers:  Some(request_headers),
            request_body:     Some(request_body),
            response_headers: Some(response_headers),
            response_body:    Some(response_body),
        };

        Ok(parse_turn(&resp, &req.model).with_raw(raw))
    }

    /// SSE streaming path. Accumulates fragments into the same `ModelResponse`
    /// the buffered path returns, forwarding deltas best-effort. `emitted`
    /// tracks whether any delta was pushed, distinguishing a pre-stream
    /// failure (safe to retry buffered) from a mid-stream one.
    async fn stream_chat(
        &self,
        req:      &ModelRequest,
        delta_tx: &mpsc::Sender<StreamDelta>,
        emitted:  &mut bool,
    ) -> Result<ModelResponse, ModelError> {
        let mut body = self.base_body(&req.model, &req.messages, &req.tools);
        body["stream"]         = json!(true);
        body["stream_options"] = json!({ "include_usage": true });
        let body = self.finalize_body(body, req);

        debug!(model = %req.model, tools = req.tools.len(), prompt_cache = self.enable_prompt_cache, "openai: sending streaming request");
        trace!(body = %body, "openai: streaming request body");

        let request_body    = body.clone();
        let request_headers = self.logged_headers();

        let http_resp = self.send_request(&body).await?;

        let response_headers = headers_to_json(http_resp.headers());
        let status           = http_resp.status();
        if !status.is_success() {
            let resp_text = http_resp.text().await.map_err(ModelError::from_reqwest)?;
            return Err(ModelError {
                status:  Some(status.as_u16()),
                message: format!("openai: HTTP {status} from {url}\nbody: {resp_text}", url = self.url()),
                raw:     Some(RawMeta {
                    request_headers:  Some(request_headers),
                    request_body:     Some(request_body),
                    response_headers: Some(response_headers),
                    response_body:    Some(error_response_body(resp_text)),
                }),
            });
        }

        let mut content       = String::new();
        let mut reasoning     = String::new();
        // index → (id, name, arguments fragment buffer)
        let mut tool_calls: BTreeMap<u64, (String, String, String)> = BTreeMap::new();
        let mut finish_reason: Option<String> = None;
        let mut usage: Option<Value> = None;
        let mut sse = SseDecoder::new();
        let mut byte_stream = http_resp.bytes_stream();

        let mut handle_payload = |payload: &str, emitted: &mut bool| {
            if payload == "[DONE]" {
                return;
            }
            let Ok(v) = serde_json::from_str::<Value>(payload) else { return };
            if let Some(u) = v.get("usage").filter(|u| !u.is_null()) {
                usage = Some(u.clone());
            }
            let Some(choice) = v["choices"].as_array().and_then(|a| a.first()) else { return };
            if let Some(fr) = choice["finish_reason"].as_str() {
                finish_reason = Some(fr.to_string());
            }
            let delta = &choice["delta"];
            if let Some(t) = delta["content"].as_str().filter(|t| !t.is_empty()) {
                content.push_str(t);
                *emitted = true;
                let _ = delta_tx.try_send(StreamDelta::Text(t.to_string()));
            }
            // DeepSeek uses `reasoning_content`, MiniMax M3 and others `reasoning`.
            if let Some(t) = delta["reasoning_content"].as_str()
                .or_else(|| delta["reasoning"].as_str())
                .filter(|t| !t.is_empty())
            {
                reasoning.push_str(t);
                *emitted = true;
                let _ = delta_tx.try_send(StreamDelta::Reasoning(t.to_string()));
            }
            if let Some(tc_arr) = delta["tool_calls"].as_array() {
                for tc in tc_arr {
                    let idx = tc["index"].as_u64().unwrap_or(0);
                    let entry = tool_calls.entry(idx).or_default();
                    if let Some(id) = tc["id"].as_str() { entry.0 = id.to_string(); }
                    if let Some(n) = tc["function"]["name"].as_str() { entry.1 = n.to_string(); }
                    if let Some(a) = tc["function"]["arguments"].as_str() { entry.2.push_str(a); }
                }
            }
        };

        while let Some(chunk) = byte_stream.next().await {
            let chunk = chunk.map_err(ModelError::from_reqwest)?;
            for payload in sse.feed(&chunk) {
                handle_payload(&payload, emitted);
            }
        }
        for payload in sse.finish() {
            handle_payload(&payload, emitted);
        }

        let finish          = finish_reason.as_deref().unwrap_or("stop");
        let input_tokens    = usage.as_ref().and_then(|u| u["prompt_tokens"].as_u64()).map(|n| n as u32);
        let output_tokens   = usage.as_ref().and_then(|u| u["completion_tokens"].as_u64()).map(|n| n as u32);
        let cache_read = usage.as_ref()
            .and_then(|u| u["prompt_tokens_details"]["cached_tokens"].as_u64())
            .map(|n| n as u32);
        let cost_usd        = usage.as_ref().and_then(|u| u["cost"].as_f64());
        let reasoning_content = if reasoning.is_empty() { None } else { Some(reasoning) };
        info!(model = %req.model, ?input_tokens, ?output_tokens, finish_reason = finish, "openai: streaming response completed");
        if finish == "length" {
            warn!(model = %req.model, ?output_tokens, "openai: response truncated (max_tokens reached)");
        }

        let usage_struct = Usage {
            input_tokens,
            output_tokens,
            cache_read,
            cache_write: None,
            cost_usd,
            truncated: finish == "length",
        };

        // Reassemble the streamed message for the payload log (buffered shape).
        let logged_tool_calls: Vec<Value> = tool_calls.iter()
            .map(|(_idx, (id, name, args))| json!({
                "id":   id,
                "type": "function",
                "function": { "name": name, "arguments": args },
            }))
            .collect();
        let mut logged_message = json!({ "role": "assistant", "content": content.clone() });
        if let Some(rc) = &reasoning_content {
            logged_message["reasoning_content"] = rc.clone().into();
        }
        if !logged_tool_calls.is_empty() {
            logged_message["tool_calls"] = Value::Array(logged_tool_calls);
        }
        let raw = RawMeta {
            request_headers:  Some(request_headers),
            request_body:     Some(request_body),
            response_headers: Some(response_headers),
            response_body:    Some(json!({
                "streamed": true,
                "choices": [{ "finish_reason": finish, "message": logged_message }],
                "usage":   usage,
            })),
        };

        let mut resp = if !tool_calls.is_empty() {
            let calls = tool_calls
                .into_values()
                .map(|(id, name, args)| ToolCall {
                    id,
                    name,
                    arguments: serde_json::from_str(&args).unwrap_or(Value::Object(Default::default())),
                })
                .collect();
            ModelResponse::ToolCalls { content, calls, reasoning: reasoning_content, usage: usage_struct, raw: None }
        } else {
            ModelResponse::Message { content, reasoning: reasoning_content, usage: usage_struct, raw: None }
        };
        set_raw(&mut resp, raw);
        Ok(resp)
    }
}

impl NamedModel for OpenAiModel {
    fn default_model(&self) -> &str { &self.default_model }
}

#[async_trait]
impl Model for OpenAiModel {
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
                    // Nothing was ever streamed: some OpenAI-compatible
                    // providers reject `stream`/`stream_options` outright —
                    // retry buffered so they keep working. A mid-stream
                    // failure instead propagates to the fallback logic.
                    Err(e) if !emitted => {
                        debug!(model = %req.model, error = %e, "openai: streaming failed before any delta; retrying buffered");
                        self.buffered(req).await
                    }
                    Err(e) => Err(e),
                }
            }
        }
    }
}

// ── response parsing (shared by buffered and tests) ──

trait WithRaw {
    fn with_raw(self, raw: RawMeta) -> ModelResponse;
}

impl WithRaw for ModelResponse {
    fn with_raw(mut self, raw: RawMeta) -> ModelResponse {
        set_raw(&mut self, raw);
        self
    }
}

fn set_raw(resp: &mut ModelResponse, raw: RawMeta) {
    match resp {
        ModelResponse::Message { raw: r, .. } | ModelResponse::ToolCalls { raw: r, .. } => {
            *r = Some(raw)
        }
    }
}

/// Parse a buffered OpenAI response body into a `ModelResponse`.
fn parse_turn(resp: &Value, model: &str) -> ModelResponse {
    let usage = Usage {
        input_tokens:  resp["usage"]["prompt_tokens"].as_u64().map(|n| n as u32),
        output_tokens: resp["usage"]["completion_tokens"].as_u64().map(|n| n as u32),
        cache_read:    resp["usage"]["prompt_tokens_details"]["cached_tokens"].as_u64().map(|n| n as u32),
        cache_write:   None,
        cost_usd:      resp["usage"]["cost"].as_f64(),
        truncated:     false,
    };

    let choice  = &resp["choices"][0];
    let message = &choice["message"];
    let finish  = choice["finish_reason"].as_str().unwrap_or("stop");
    if finish == "length" {
        warn!(model = %model, "openai: response truncated (max_tokens reached)");
    }

    let reasoning_content = message["reasoning_content"].as_str()
        .or_else(|| message["reasoning"].as_str())
        .map(str::to_string);

    let tool_calls_array = message["tool_calls"].as_array().filter(|a| !a.is_empty());

    // Some models (e.g. Qwen via OpenRouter) return finish_reason "stop" even
    // when tool_calls are present, so check the array directly.
    if finish == "tool_calls" || tool_calls_array.is_some() {
        let content = message["content"].as_str().unwrap_or("").to_string();
        let calls = tool_calls_array
            .map(|arr| {
                arr.iter()
                    .map(|tc| ToolCall {
                        id:   tc["id"].as_str().unwrap_or("").to_string(),
                        name: tc["function"]["name"].as_str().unwrap_or("").to_string(),
                        arguments: tc["function"]["arguments"]
                            .as_str()
                            .and_then(|s| serde_json::from_str(s).ok())
                            .unwrap_or(Value::Object(Default::default())),
                    })
                    .collect()
            })
            .unwrap_or_default();
        ModelResponse::ToolCalls { content, calls, reasoning: reasoning_content, usage, raw: None }
    } else {
        // content can be null for thinking models or finish_reason="length".
        let content = match message["content"].as_str() {
            Some(s) => s.to_string(),
            None => {
                warn!(finish_reason = finish, raw_message = %message, "openai: response has null content");
                String::new()
            }
        };
        let mut usage = usage;
        usage.truncated = finish == "length";
        ModelResponse::Message { content, reasoning: reasoning_content, usage, raw: None }
    }
}
