use std::collections::BTreeMap;

use async_trait::async_trait;
use futures_util::StreamExt;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tracing::{debug, info, trace, warn};

use crate::{ChatOptions, ChatResponse, ChatbotClient, LlmRawMeta, LlmTurn, Message, Role, SseDecoder, StreamDelta, ToolCall, error_response_body, headers_to_json, redact_key};
use core_api::APP_NAME;

/// OpenAI ChatGPT client (also compatible with any OpenAI-spec endpoint).
pub struct OpenAiClient {
    base_url:            String,
    api_key:             String,
    extra_params:        Option<serde_json::Value>,
    /// When true, Anthropic-compatible prompt-caching hints are injected:
    /// - `anthropic-beta: prompt-caching-2024-07-31` header is sent.
    /// - The last tool definition is tagged with `cache_control: {"type":"ephemeral"}`.
    /// - System message content is expected to already be a content array with
    ///   `cache_control` on the static block (set by `build_openai_messages`).
    /// Used for OpenRouter when routing to Anthropic models.
    enable_prompt_cache: bool,
    http:                reqwest::Client,
}

impl OpenAiClient {
    pub fn new(base_url: impl Into<String>, api_key: impl Into<String>, extra_params: Option<serde_json::Value>, enable_prompt_cache: bool) -> Self {
        Self {
            base_url:            base_url.into(),
            api_key:             api_key.into(),
            extra_params,
            enable_prompt_cache,
            http:                reqwest::Client::new(),
        }
    }

    /// Merges `extra_params` (if any) into `body`. Only top-level object keys are merged.
    fn apply_extra(&self, body: &mut serde_json::Value) {
        if let Some(serde_json::Value::Object(extra)) = &self.extra_params {
            if let Some(b) = body.as_object_mut() {
                for (k, v) in extra {
                    b.insert(k.clone(), v.clone());
                }
            }
        }
    }

    fn url(&self) -> String {
        format!("{}/chat/completions", self.base_url.trim_end_matches('/'))
    }

    /// Shared request body for the buffered and the streaming path. Caller adds
    /// `max_tokens`/`temperature`/`extra_params` afterwards via `finalize_body`.
    fn base_body(&self, model: &str, messages: &[Value], tools: &[Value]) -> Value {
        let mut body = json!({
            "model":    model,
            "messages": messages,
        });

        if !tools.is_empty() {
            // When prompt caching is enabled, tag the last tool with cache_control
            // so the entire tools array is included in the Anthropic KV cache prefix.
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

    fn finalize_body(&self, mut body: Value, options: &ChatOptions) -> Value {
        if let Some(t) = options.max_tokens  { body["max_tokens"]  = t.into(); }
        if let Some(t) = options.temperature { body["temperature"] = t.into(); }
        self.apply_extra(&mut body);
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

    async fn send_request(&self, body: &Value) -> reqwest::Result<reqwest::Response> {
        let mut req = self.http.post(self.url()).bearer_auth(&self.api_key).header("X-Title", APP_NAME);
        if self.enable_prompt_cache {
            req = req.header("anthropic-beta", "prompt-caching-2024-07-31");
        }
        req.json(body).send().await
    }

    /// SSE streaming path behind `chat_with_tools_raw_streaming`. Accumulates
    /// content/reasoning/tool-call fragments into the same `LlmTurn` the
    /// buffered path would return, while forwarding text/reasoning deltas to
    /// `delta_tx` (try_send, best-effort). `emitted` tracks whether any delta
    /// was pushed, so the caller can distinguish a pre-stream failure (safe to
    /// retry buffered) from a mid-stream one (partial output already shown).
    async fn stream_chat(
        &self,
        messages: &[Value],
        tools:    &[Value],
        options:  &ChatOptions,
        delta_tx: &mpsc::Sender<StreamDelta>,
        emitted:  &mut bool,
    ) -> anyhow::Result<(LlmTurn, Option<LlmRawMeta>)> {
        let mut body = self.base_body(&options.model, messages, tools);
        body["stream"]         = json!(true);
        body["stream_options"] = json!({ "include_usage": true });
        let body = self.finalize_body(body, options);

        debug!(model = %options.model, tools = tools.len(), prompt_cache = self.enable_prompt_cache, "openai: sending streaming chat_with_tools request");
        trace!(body = %body, "openai: streaming chat_with_tools request body");

        let request_body    = body.clone();
        let request_headers = self.logged_headers();

        let http_resp = self.send_request(&body).await?;

        let response_headers = headers_to_json(http_resp.headers());
        let status           = http_resp.status();
        if !status.is_success() {
            let resp_text = http_resp.text().await?;
            return Err(crate::LlmError {
                status:  Some(status.as_u16()),
                message: format!(
                    "openai: HTTP {status} from {url}\nbody: {resp_text}",
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

        let mut content       = String::new();
        let mut reasoning     = String::new();
        // index → (id, name, arguments fragment buffer)
        let mut tool_calls: BTreeMap<u64, (String, String, String)> = BTreeMap::new();
        let mut finish_reason: Option<String> = None;
        let mut usage: Option<Value> = None;
        let mut sse = SseDecoder::new();
        let mut byte_stream = http_resp.bytes_stream();

        // One SSE `data:` payload. Fragments update the accumulators; text and
        // reasoning also go out as deltas. Unparseable chunks are skipped —
        // the assembled turn stays consistent.
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
            // Same normalization as the buffered path: DeepSeek uses
            // `reasoning_content`, MiniMax M3 and others `reasoning`.
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
            let chunk = chunk?;
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
        let cache_read_tokens = usage.as_ref()
            .and_then(|u| u["prompt_tokens_details"]["cached_tokens"].as_u64())
            .map(|n| n as u32);
        let cost            = usage.as_ref().and_then(|u| u["cost"].as_f64());
        let reasoning_content = if reasoning.is_empty() { None } else { Some(reasoning) };
        info!(model = %options.model, ?input_tokens, ?output_tokens, finish_reason = finish, "openai: streaming response completed");
        if finish == "length" {
            warn!(model = %options.model, ?output_tokens, "openai: response truncated (max_tokens reached)");
        }

        let turn = if !tool_calls.is_empty() {
            let calls = tool_calls
                .into_values()
                .map(|(id, name, args)| ToolCall {
                    id,
                    name,
                    arguments: serde_json::from_str(&args).unwrap_or(Value::Object(Default::default())),
                })
                .collect();
            LlmTurn::ToolCalls { content, calls, input_tokens, output_tokens, reasoning_content, cache_read_tokens, cache_creation_tokens: None, cost }
        } else {
            let truncated = finish == "length";
            LlmTurn::Message(ChatResponse { content, input_tokens, output_tokens, truncated, reasoning_content, cache_read_tokens, cache_creation_tokens: None, cost })
        };

        // Synthesize a buffered-shaped response body for the payload log, so a
        // streamed call leaves the same debugging trail as a buffered one.
        let response_body = json!({
            "streamed": true,
            "choices": [{ "finish_reason": finish }],
            "usage":   usage,
        });
        let raw_meta = LlmRawMeta {
            request_headers:  Some(request_headers),
            request_body:     Some(request_body),
            response_headers: Some(response_headers),
            response_body:    Some(response_body),
        };

        Ok((turn, Some(raw_meta)))
    }
}

#[async_trait]
impl ChatbotClient for OpenAiClient {
    async fn chat(
        &self,
        messages: &[Message],
        options:  &ChatOptions,
    ) -> anyhow::Result<ChatResponse> {
        let msgs: Vec<Value> = messages
            .iter()
            .map(|m| {
                let role = match m.role {
                    Role::System    => "system",
                    Role::User      => "user",
                    Role::Assistant => "assistant",
                };
                json!({ "role": role, "content": m.content })
            })
            .collect();

        let mut body = json!({
            "model":    options.model,
            "messages": msgs,
        });

        if let Some(t) = options.max_tokens  { body["max_tokens"]  = t.into(); }
        if let Some(t) = options.temperature { body["temperature"] = t.into(); }
        self.apply_extra(&mut body);

        debug!(model = %options.model, "openai: sending chat request");
        trace!(body = %body, "openai: chat request body");

        let resp: Value = self
            .http
            .post(self.url())
            .bearer_auth(&self.api_key)
            .header("X-Title", APP_NAME)
            .json(&body)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;

        let content = match resp["choices"][0]["message"]["content"].as_str() {
            Some(s) => s.to_string(),
            None => {
                warn!(raw_response = %resp, "openai: chat() response has null content");
                String::new()
            }
        };

        let input_tokens      = resp["usage"]["prompt_tokens"].as_u64().map(|n| n as u32);
        let output_tokens     = resp["usage"]["completion_tokens"].as_u64().map(|n| n as u32);
        let cache_read_tokens = resp["usage"]["prompt_tokens_details"]["cached_tokens"].as_u64().map(|n| n as u32);
        let truncated         = resp["choices"][0]["finish_reason"].as_str() == Some("length");
        let cost              = self.extract_cost(&resp);
        info!(model = %options.model, ?input_tokens, ?output_tokens, ?cost, truncated, "openai: chat response received");

        Ok(ChatResponse { content, input_tokens, output_tokens, truncated, reasoning_content: None, cache_read_tokens, cache_creation_tokens: None, cost })
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
        let body = self.finalize_body(self.base_body(&options.model, messages, tools), options);

        debug!(model = %options.model, tools = tools.len(), prompt_cache = self.enable_prompt_cache, "openai: sending chat_with_tools request");
        trace!(body = %body, "openai: chat_with_tools request body");

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
                    "openai: HTTP {status} from {url}\nbody: {resp_text}",
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
            .map_err(|e| anyhow::anyhow!("openai: failed to parse response JSON: {e}\nbody: {resp_text}"))?;
        let response_body: Value = serde_json::from_str(&resp_text).unwrap_or(Value::Null);

        let raw_meta = LlmRawMeta {
            request_headers:  Some(request_headers),
            request_body:     Some(request_body),
            response_headers: Some(response_headers),
            response_body:    Some(response_body),
        };

        let input_tokens      = resp["usage"]["prompt_tokens"].as_u64().map(|n| n as u32);
        let output_tokens     = resp["usage"]["completion_tokens"].as_u64().map(|n| n as u32);
        let cache_read_tokens = resp["usage"]["prompt_tokens_details"]["cached_tokens"].as_u64().map(|n| n as u32);
        let cost              = self.extract_cost(&resp);

        let choice  = &resp["choices"][0];
        let message = &choice["message"];
        let finish  = choice["finish_reason"].as_str().unwrap_or("stop");
        info!(model = %options.model, ?input_tokens, ?output_tokens, finish_reason = finish, "openai: chat_with_tools response received");
        if finish == "length" {
            warn!(model = %options.model, ?output_tokens, "openai: response truncated (max_tokens reached)");
        }

        // Thinking/reasoning content varies by provider:
        //   - DeepSeek:  "reasoning_content" (must be echoed back on subsequent turns, even as "")
        //   - MiniMax M3 and others: "reasoning"
        // We normalize to a single field and echo under both names in message_builder.
        let reasoning_content = message["reasoning_content"].as_str()
            .or_else(|| message["reasoning"].as_str())
            .map(str::to_string);

        let tool_calls_array = message["tool_calls"].as_array().filter(|a| !a.is_empty());

        // Some models (e.g. Qwen via OpenRouter) return finish_reason "stop" even when
        // tool_calls are present, so check the array directly rather than relying on finish_reason.
        let turn = if finish == "tool_calls" || tool_calls_array.is_some() {
            let content = message["content"].as_str().unwrap_or("").to_string();

            let calls = tool_calls_array
                .ok_or_else(|| anyhow::anyhow!("finish_reason=tool_calls but tool_calls array missing or empty"))?
                .iter()
                .map(|tc| {
                    let id   = tc["id"].as_str().unwrap_or("").to_string();
                    let name = tc["function"]["name"].as_str().unwrap_or("").to_string();
                    let args: Value = tc["function"]["arguments"]
                        .as_str()
                        .and_then(|s| serde_json::from_str(s).ok())
                        .unwrap_or(Value::Object(Default::default()));
                    ToolCall { id, name, arguments: args }
                })
                .collect();

            LlmTurn::ToolCalls { content, calls, input_tokens, output_tokens, reasoning_content, cache_read_tokens, cache_creation_tokens: None, cost }
        } else {
            // content can be null for thinking/reasoning models or when finish_reason="length".
            // Fall back to empty string rather than erroring — the partial response is still
            // useful and a hard error breaks the session.
            let content = match message["content"].as_str() {
                Some(s) => s.to_string(),
                None => {
                    tracing::warn!(
                        finish_reason = finish,
                        ?input_tokens,
                        ?output_tokens,
                        raw_message = %message,
                        "OpenAI response has null content",
                    );
                    String::new()
                }
            };
            let truncated = finish == "length";
            LlmTurn::Message(ChatResponse { content, input_tokens, output_tokens, truncated, reasoning_content, cache_read_tokens, cache_creation_tokens: None, cost })
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
            // Nothing was ever streamed: some OpenAI-compatible providers reject
            // `stream`/`stream_options` outright — retry buffered so they keep
            // working exactly as before. A mid-stream failure (deltas already
            // shown) instead propagates to the model-fallback logic.
            Err(e) if !emitted => {
                debug!(model = %options.model, error = %e, "openai: streaming failed before any delta; retrying buffered");
                self.chat_with_tools_raw(messages, tools, options).await
            }
            Err(e) => Err(e),
        }
    }
}
