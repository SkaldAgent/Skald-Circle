use async_trait::async_trait;
use serde_json::{Value, json};
use tracing::{debug, info, trace, warn};

use crate::{ChatOptions, ChatResponse, ChatbotClient, LlmRawMeta, LlmTurn, Message, Role, ToolCall, headers_to_json, redact_key};
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
        let mut body = json!({
            "model":    options.model,
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

        if let Some(t) = options.max_tokens  { body["max_tokens"]  = t.into(); }
        if let Some(t) = options.temperature { body["temperature"] = t.into(); }
        self.apply_extra(&mut body);

        debug!(model = %options.model, tools = tools.len(), prompt_cache = self.enable_prompt_cache, "openai: sending chat_with_tools request");
        trace!(body = %body, "openai: chat_with_tools request body");

        // Capture request metadata for logging.
        let mut logged_headers = json!({
            "authorization": format!("Bearer {}", redact_key(&self.api_key)),
            "content-type":  "application/json",
        });
        if self.enable_prompt_cache {
            logged_headers["anthropic-beta"] = "prompt-caching-2024-07-31".into();
        }
        let request_body    = body.clone();
        let request_headers = logged_headers;

        let mut req = self.http.post(self.url()).bearer_auth(&self.api_key).header("X-Title", APP_NAME);
        if self.enable_prompt_cache {
            req = req.header("anthropic-beta", "prompt-caching-2024-07-31");
        }
        let http_resp = req
            .json(&body)
            .send()
            .await?;

        let response_headers = headers_to_json(http_resp.headers());
        let status           = http_resp.status();
        let resp_text        = http_resp.text().await?;

        if !status.is_success() {
            return Err(anyhow::anyhow!(
                "openai: HTTP {status} from {url}\nbody: {resp_text}",
                url = self.url(),
            ));
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
}
