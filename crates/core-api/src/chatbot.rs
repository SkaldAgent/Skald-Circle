use async_trait::async_trait;
use serde_json::Value;

/// A single message in a conversation.
#[derive(Debug, Clone)]
pub struct Message {
    pub role:    Role,
    pub content: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Role {
    System,
    User,
    Assistant,
}

impl Message {
    pub fn system(content: impl Into<String>) -> Self {
        Self { role: Role::System, content: content.into() }
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self { role: Role::User, content: content.into() }
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self { role: Role::Assistant, content: content.into() }
    }
}

/// Options for a single chat completion request.
#[derive(Debug, Clone)]
pub struct ChatOptions {
    pub model:       String,
    pub max_tokens:  Option<u32>,
    pub temperature: Option<f32>,
    /// Session/stack IDs for request logging. Set by the LLM loop; ignored by
    /// providers — only the logging wrapper reads them.
    pub session_id:  Option<i64>,
    pub stack_id:    Option<i64>,
}

/// Raw HTTP metadata captured during a provider call.
/// Sensitive header values (api_key) are redacted before storage.
#[derive(Debug, Default)]
pub struct LlmRawMeta {
    pub request_headers:  Option<Value>,
    pub request_body:     Option<Value>,
    pub response_headers: Option<Value>,
    pub response_body:    Option<Value>,
}

/// The response from a chat completion (text only).
#[derive(Debug, Clone)]
pub struct ChatResponse {
    pub content:              String,
    pub input_tokens:         Option<u32>,
    pub output_tokens:        Option<u32>,
    /// True when the model stopped due to hitting the token limit.
    pub truncated:            bool,
    /// Chain-of-thought produced by reasoning models (e.g. DeepSeek thinking mode).
    /// Must be echoed back in the assistant message on subsequent turns.
    pub reasoning_content:    Option<String>,
    /// Tokens served from the provider's prompt cache (Anthropic: cache_read_input_tokens,
    /// OpenAI: prompt_tokens_details.cached_tokens). None when the provider does not
    /// report cache metrics.
    pub cache_read_tokens:    Option<u32>,
    /// Tokens written into the provider's prompt cache (Anthropic only:
    /// cache_creation_input_tokens). None for providers that do not expose this.
    pub cache_creation_tokens: Option<u32>,
    /// Cost of the request in USD, when the provider reports it (OpenRouter
    /// returns it under `usage.cost`). None for providers that do not bill
    /// per-request or do not expose the figure.
    pub cost:                  Option<f64>,
}

/// A single tool call requested by the LLM.
#[derive(Debug, Clone)]
pub struct ToolCall {
    pub id:        String,
    pub name:      String,
    pub arguments: Value,
}

/// Result of one LLM turn when tools are available.
#[derive(Debug)]
pub enum LlmTurn {
    Message(ChatResponse),
    ToolCalls {
        content:               String,
        calls:                 Vec<ToolCall>,
        input_tokens:          Option<u32>,
        output_tokens:         Option<u32>,
        reasoning_content:     Option<String>,
        cache_read_tokens:     Option<u32>,
        cache_creation_tokens: Option<u32>,
        cost:                  Option<f64>,
    },
}

/// Stateless LLM client. Implementations hold only connection config (base URL,
/// API key). No memory, no database, no session state.
#[async_trait]
pub trait ChatbotClient: Send + Sync {
    async fn chat(
        &self,
        messages: &[Message],
        options:  &ChatOptions,
    ) -> anyhow::Result<ChatResponse>;

    /// Extracts the request cost in USD from a provider's raw JSON response,
    /// when the provider reports it. OpenRouter (and other OpenAI-compatible
    /// gateways) return it under `usage.cost`; the default reads that path and
    /// yields None when absent. Providers with a different shape override this.
    fn extract_cost(&self, response: &Value) -> Option<f64> {
        response["usage"]["cost"].as_f64()
    }

    /// Chat with tool support. Default implementation ignores tools and falls
    /// back to `chat()`.
    async fn chat_with_tools(
        &self,
        messages: &[Value],
        tools:    &[Value],
        options:  &ChatOptions,
    ) -> anyhow::Result<LlmTurn> {
        let simple: Vec<Message> = messages
            .iter()
            .filter_map(|m| {
                let role    = m["role"].as_str()?;
                let content = m["content"].as_str().unwrap_or("").to_string();
                match role {
                    "system"    => Some(Message::system(content)),
                    "user"      => Some(Message::user(content)),
                    "assistant" => Some(Message::assistant(content)),
                    _           => None,
                }
            })
            .collect();
        let _ = tools;
        let resp = self.chat(&simple, options).await?;
        Ok(LlmTurn::Message(resp))
    }

    /// Like `chat_with_tools` but also returns raw HTTP metadata for logging.
    /// Providers that make real HTTP calls should override this.
    async fn chat_with_tools_raw(
        &self,
        messages: &[Value],
        tools:    &[Value],
        options:  &ChatOptions,
    ) -> anyhow::Result<(LlmTurn, Option<LlmRawMeta>)> {
        self.chat_with_tools(messages, tools, options).await.map(|t| (t, None))
    }
}
