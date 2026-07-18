pub mod anthropic;
pub mod declared;
pub mod ollama;
pub mod openai;
pub mod openrouter;

// Re-export so existing code that uses `providers::ServiceType` / `providers::RemoteLlmModelInfo` keeps working.
pub use crate::provider::ServiceType;
pub use core_api::provider::RemoteLlmModelInfo;

use std::sync::Arc;

use anyhow::{anyhow, Context, Result};

use core_api::provider::{ApiProvider, BuiltLlmClient, LlmModelRecord, LlmProviderRecord};

use crate::chatbot::openai::OpenAiClient;

/// Computes the `extra_params` an OpenAI-compatible client should be built with,
/// given a model's stored `extra_params` and its selected reasoning value. The
/// provider translates the reasoning value into a request fragment via
/// `reasoning_request`; that fragment's top-level keys are merged over
/// `extra_params` (reasoning wins on conflict). Returns `None` when neither is set.
pub(crate) fn extra_with_reasoning(
    provider: &dyn ApiProvider,
    model:    &LlmModelRecord,
) -> Option<serde_json::Value> {
    let reasoning = model.reasoning.as_ref().and_then(|v| provider.reasoning_request(v));
    match (model.extra_params.clone(), reasoning) {
        (base, None)          => base,
        (None, overlay)       => overlay,
        (Some(mut base), Some(overlay)) => {
            match (base.as_object_mut(), overlay.as_object()) {
                (Some(b), Some(o)) => {
                    for (k, v) in o { b.insert(k.clone(), v.clone()); }
                    Some(base)
                }
                // Non-object base: the reasoning overlay takes precedence.
                _ => Some(overlay),
            }
        }
    }
}

/// Fetches the OpenAI-style `GET {base_url}/models` catalog shared by most
/// OpenAI-compatible providers. Returns the raw per-model JSON objects from
/// the `data` envelope so each provider can map and enrich them with its own
/// heuristics. `api_key` is sent as a bearer token when present (local
/// providers pass `None`); `who` is the display name used in error messages.
pub(crate) async fn fetch_openai_models(
    http:     &reqwest::Client,
    base_url: &str,
    api_key:  Option<&str>,
    who:      &str,
) -> Result<Vec<serde_json::Value>> {
    let url = format!("{}/models", base_url.trim_end_matches('/'));
    let mut req = http.get(&url);
    if let Some(key) = api_key {
        req = req.bearer_auth(key);
    }
    let resp: serde_json::Value = req
        .send()
        .await
        .map_err(|e| anyhow!("{who} request failed: {e}"))?
        .error_for_status()
        .map_err(|e| anyhow!("{who} error response: {e}"))?
        .json()
        .await
        .map_err(|e| anyhow!("{who} response parse failed: {e}"))?;

    resp["data"]
        .as_array()
        .cloned()
        .ok_or_else(|| anyhow!("unexpected {who} response shape"))
}

/// Builds an `OpenAiClient` for an OpenAI-compatible provider: requires the
/// provider record's `api_key` and merges the model's stored `extra_params`
/// with the provider-translated reasoning fragment (see `extra_with_reasoning`).
pub(crate) fn build_openai_llm(
    provider:     &dyn ApiProvider,
    base_url:     &str,
    record:       &LlmProviderRecord,
    model:        &LlmModelRecord,
    prompt_cache: bool,
) -> Result<BuiltLlmClient> {
    let key = record.api_key.as_deref()
        .with_context(|| format!("provider '{}': api_key required for {}", record.name, provider.type_id()))?;
    let extra = extra_with_reasoning(provider, model);
    Ok(BuiltLlmClient {
        client: Arc::new(OpenAiClient::new(base_url, key, extra, prompt_cache)),
        prompt_cache,
    })
}
