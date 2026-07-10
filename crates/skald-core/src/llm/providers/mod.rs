pub mod anthropic;
pub mod deepseek;
pub mod lm_studio;
pub mod ollama;
pub mod openai;
pub mod openrouter;
pub mod zai;

// Re-export so existing code that uses `providers::ServiceType` / `providers::RemoteLlmModelInfo` keeps working.
pub use crate::provider::ServiceType;
pub use core_api::provider::RemoteLlmModelInfo;

use core_api::provider::{ApiProvider, LlmModelRecord};

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
