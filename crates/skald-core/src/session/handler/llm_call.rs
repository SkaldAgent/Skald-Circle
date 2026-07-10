//! One LLM call per round, with automatic model fallback.
//!
//! Extracted from `run_agent_turn`: on a retriable error (5xx / network) it retries
//! up to `MAX_LLM_ATTEMPTS` models in priority order, rebuilding the message list
//! when the replacement model has a different `prompt_cache` setting, and emits
//! `ModelFallback` / `LlmFailed` along the way.

use std::collections::HashSet;
use std::sync::Arc;

use serde_json::Value;
use tokio_util::sync::CancellationToken;
use tracing::{error, warn};

use crate::chatbot::{ChatOptions, LlmTurn};
use crate::db::llm_request_payloads;
use crate::llm::{LlmEntry, LlmStrength};

use super::ChatSessionHandler;
use super::emitter::TurnEmitter;
use super::interface_tools::AgentRunConfig;

/// Outcome of one round's LLM call.
pub(super) enum RoundLlm {
    /// The model responded (message or tool calls).
    Turn(LlmTurn),
    /// The turn was cancelled (`/stop`) while the request was in flight.
    Cancelled,
    /// All fallback attempts were exhausted, or an error is non-retriable.
    Failed(anyhow::Error),
}

/// Maximum number of models tried in one round before giving up.
const MAX_LLM_ATTEMPTS: usize = 3;

impl ChatSessionHandler {
    /// Calls the current model and, on a retriable failure, falls back to the next
    /// model in priority order. Mutates `cur_name` / `cur_llm` / `messages` in place
    /// so the caller keeps using the model that actually produced the turn.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn call_llm_round(
        &self,
        stack_id:      i64,
        config:        &AgentRunConfig,
        active_grants: &HashSet<String>,
        tool_defs:     &[Value],
        req_scope:     Option<&str>,
        req_strength:  Option<LlmStrength>,
        cur_name:      &mut String,
        cur_llm:       &mut Arc<LlmEntry>,
        messages:      &mut Vec<Value>,
        token:         &CancellationToken,
        em:            &TurnEmitter<'_>,
    ) -> RoundLlm {
        let mut tried_this_round: Vec<String> = vec![cur_name.clone()];

        loop {
            let request_id = uuid::Uuid::new_v4().to_string();
            let options = ChatOptions {
                model:       cur_llm.model.clone(),
                max_tokens:  None,
                temperature: None,
                session_id:  Some(self.session_id),
                stack_id:    Some(stack_id),
                user_id:     Some(self.user_id.clone()),
                request_id:  Some(request_id.clone()),
            };

            // Clone the Arc so the in-flight future does not borrow `cur_llm` across
            // the fallback reassignment below. On cancel we drop the future
            // (aborting the request) and return immediately.
            let client = cur_llm.client.clone();
            let call_result = tokio::select! {
                _ = token.cancelled() => return RoundLlm::Cancelled,
                r = client.chat_with_tools_raw(messages.as_slice(), tool_defs, &options) => r,
            };

            let e = match call_result {
                Ok((turn, meta)) => {
                    self.llm_manager.mark_success(cur_name).await;
                    // Persist the payload (request/response bodies + headers) to the
                    // user's own database. Fire-and-forget — a failed write must not
                    // break the turn. The metadata row is already written by the
                    // logging wrapper to system.db with the same request_id.
                    if let Some(meta) = meta {
                        let pool = Arc::clone(&self.db);
                        let rid  = request_id.clone();
                        tokio::spawn(async move {
                            let row = llm_request_payloads::PayloadRow {
                                request_id:       rid,
                                request_json:     meta.request_body.map(|v| v.to_string()).unwrap_or_default(),
                                request_headers:  meta.request_headers.map(|v| v.to_string()),
                                response_json:    meta.response_body.map(|v| v.to_string()),
                                response_headers: meta.response_headers.map(|v| v.to_string()),
                            };
                            if let Err(e) = llm_request_payloads::insert(&pool, row).await {
                                tracing::warn!(error = %e, "llm_request_payloads: failed to insert");
                            }
                        });
                    }
                    return RoundLlm::Turn(turn);
                }
                Err(e) => e,
            };

            error!(session_id = self.session_id, client = %cur_name, error = %e, "LLM call failed");
            self.llm_manager.mark_failure(cur_name, &e.to_string()).await;

            let can_fallback = tried_this_round.len() < MAX_LLM_ATTEMPTS
                && is_retriable_llm_error(&e);
            if !can_fallback {
                em.llm_failed(tried_this_round.clone(), e.to_string()).await;
                return RoundLlm::Failed(e);
            }

            let excluded: Vec<&str> = tried_this_round.iter().map(String::as_str).collect();
            match self.llm_manager.select_excluding(&excluded, req_scope, req_strength).await {
                Ok((next_name, next_llm)) => {
                    warn!(session_id = self.session_id, from = %cur_name, to = %next_name, "LLM fallback");
                    em.model_fallback(cur_name.clone(), next_name.clone(), first_line(&e.to_string())).await;
                    tried_this_round.push(next_name.clone());
                    *cur_name = next_name;
                    *cur_llm  = next_llm;
                    // Rebuild messages if the new model uses different prompt_cache
                    // settings (e.g. switching from OpenRouter/Anthropic to DeepSeek).
                    match self.build_openai_messages(
                        &self.db, stack_id, &config.agent_id,
                        config.extra_system.as_deref(), config.extra_system_dynamic.as_deref(),
                        config.tail_reminder.as_deref(), active_grants,
                        &config.system_substitutions, cur_llm.prompt_cache,
                    ).await {
                        Ok(m)  => *messages = m,
                        Err(e) => return RoundLlm::Failed(e),
                    }
                }
                Err(_) => {
                    em.llm_failed(tried_this_round.clone(), e.to_string()).await;
                    return RoundLlm::Failed(e);
                }
            }
        }
    }
}

/// Whether an LLM error is worth retrying on a different model.
fn is_retriable_llm_error(e: &anyhow::Error) -> bool {
    let msg = e.to_string().to_lowercase();
    // Never retry client errors — the request itself is malformed or unauthorized.
    // 400 is excluded: some providers reject valid requests that others accept
    // (e.g. DeepSeek requires reasoning_content echo, OpenAI does not), so
    // retrying on a different model can succeed.
    for code in ["401", "403", "404", "422"] {
        if msg.contains(code) {
            return false;
        }
    }
    true
}

fn first_line(s: &str) -> String {
    s.lines().next().unwrap_or(s).to_string()
}
