//! One LLM call per round, with automatic model fallback.
//!
//! Extracted from `run_agent_turn`: on a retriable error (5xx / network) it retries
//! up to `MAX_LLM_ATTEMPTS` models in priority order, rebuilding the message list
//! when the replacement model has a different `prompt_cache` setting, and emits
//! `ModelFallback` / `LlmFailed` along the way.

use std::collections::HashSet;
use std::sync::Arc;

use serde_json::Value;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{error, warn};

use crate::chatbot::{ChatOptions, LlmError, LlmTurn, StreamDelta};
use crate::db::llm_request_payloads;
use crate::events::{ServerEvent, TokenDeltaKind};
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

            // Tell the model, in read_file's description, which media formats it can
            // open directly — keyed on the model actually serving this attempt, so a
            // fallback to a text-only model drops the claim. `None` (no media
            // capability) leaves the shared defs untouched, avoiding a clone.
            let annotated = media_annotated_tools(tool_defs, &cur_llm.capabilities);
            let defs: &[Value] = annotated.as_deref().unwrap_or(tool_defs);

            // Clone the Arc so the in-flight future does not borrow `cur_llm` across
            // the fallback reassignment below. On cancel we drop the future
            // (aborting the request) and return immediately.
            let client = cur_llm.client.clone();
            // Streaming side-channel: providers that support SSE push deltas here;
            // the forwarder re-emits them as `TokenDelta` events on the turn bus.
            // Best-effort — the round's final events remain authoritative.
            let (delta_tx, delta_rx) = mpsc::channel::<StreamDelta>(256);
            let forwarder = spawn_delta_forwarder(delta_rx, em.sender());
            let call_result = tokio::select! {
                _ = token.cancelled() => return RoundLlm::Cancelled,
                r = client.chat_with_tools_raw_streaming(messages.as_slice(), defs, &options, delta_tx) => r,
            };
            // The client's sender dropped with the completed future: the forwarder
            // drains any queued deltas and exits, so every `TokenDelta` precedes the
            // round's outcome events (Thinking / Done) in bus order.
            forwarder.await.ok();

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

            // Persist the payload even on failure so the debug log shows the request
            // that was rejected (e.g. a provider 400). Only the HTTP clients attach a
            // body (`LlmError::raw_meta`); a network/parse/cancel error carries none.
            // Fire-and-forget, keyed on the same `request_id` as the metadata row the
            // logging wrapper wrote to system.db.
            if let Some(meta) = e.downcast_ref::<LlmError>().and_then(|le| le.raw_meta.as_ref()) {
                let row = llm_request_payloads::PayloadRow {
                    request_id:       request_id.clone(),
                    request_json:     meta.request_body.as_ref().map(|v| v.to_string()).unwrap_or_default(),
                    request_headers:  meta.request_headers.as_ref().map(|v| v.to_string()),
                    response_json:    meta.response_body.as_ref().map(|v| v.to_string()),
                    response_headers: meta.response_headers.as_ref().map(|v| v.to_string()),
                };
                let pool = Arc::clone(&self.db);
                tokio::spawn(async move {
                    if let Err(e) = llm_request_payloads::insert(&pool, row).await {
                        tracing::warn!(error = %e, "llm_request_payloads: failed to insert error payload");
                    }
                });
            }

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
                    // settings (e.g. switching from OpenRouter/Anthropic to DeepSeek)
                    // or different input capabilities (a non-vision fallback drops
                    // inline media back to the textual path block).
                    match self.build_openai_messages(
                        &self.db, stack_id, &config.agent_id,
                        config.extra_system.as_deref(), config.extra_system_dynamic.as_deref(),
                        config.tail_reminder.as_deref(), active_grants,
                        &config.system_substitutions, cur_llm.prompt_cache, &cur_llm.capabilities,
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

/// Forwards streaming deltas from the LLM client onto the turn's event channel
/// as `TokenDelta` events. Exits when the client drops its sender (call
/// completed or aborted) or when the turn receiver is gone.
fn spawn_delta_forwarder(
    mut rx: mpsc::Receiver<StreamDelta>,
    tx: mpsc::Sender<ServerEvent>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(d) = rx.recv().await {
            let (kind, delta) = match d {
                StreamDelta::Text(t)      => (TokenDeltaKind::Content, t),
                StreamDelta::Reasoning(t) => (TokenDeltaKind::Reasoning, t),
            };
            if tx.send(ServerEvent::TokenDelta { kind, delta }).await.is_err() {
                break;
            }
        }
    })
}

/// Whether an LLM error is worth retrying on a different model.
///
/// Classifies on the real HTTP status ([`crate::chatbot::http_status`]), not a
/// substring of the message — a model id or token count containing "404"/"401" no
/// longer mis-classifies (bug B6). A non-HTTP failure (network, parse) has no status
/// and is retriable, matching the previous default.
fn is_retriable_llm_error(e: &anyhow::Error) -> bool {
    // Never retry these client errors — the request itself is unauthorized, not
    // found, or unprocessable. 400 is intentionally NOT listed: some providers
    // reject valid requests that others accept (e.g. DeepSeek requires a
    // reasoning_content echo, OpenAI does not), so retrying elsewhere can succeed.
    // 429 and 5xx stay retriable (a different model / provider may serve the call).
    !matches!(crate::chatbot::http_status(e), Some(401 | 403 | 404 | 422))
}

fn first_line(s: &str) -> String {
    s.lines().next().unwrap_or(s).to_string()
}

/// Appends a per-model media hint to `read_file`'s description when the resolved
/// model can view images/video/PDFs, so the model knows reading one of those shows
/// it the content natively. Returns `None` (leaving the shared, model-independent
/// defs untouched — no clone) when the model has no media modality. Done here, per
/// attempt, so a fallback to a different model re-derives the hint from its caps.
fn media_annotated_tools(tool_defs: &[Value], capabilities: &[String]) -> Option<Vec<Value>> {
    let hint = super::media::media_capability_hint(capabilities)?;
    let mut out = tool_defs.to_vec();
    for def in &mut out {
        if def["function"]["name"].as_str() == Some("read_file") {
            if let Some(d) = def["function"]["description"].as_str() {
                def["function"]["description"] = Value::String(format!("{d}{hint}"));
            }
            break;
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::is_retriable_llm_error;
    use crate::chatbot::LlmError;

    fn http_err(status: u16, message: &str) -> anyhow::Error {
        LlmError { status: Some(status), message: message.to_string(), ..Default::default() }.into()
    }

    #[test]
    fn client_errors_are_not_retried() {
        for code in [401, 403, 404, 422] {
            assert!(!is_retriable_llm_error(&http_err(code, "nope")), "{code} must not retry");
        }
    }

    #[test]
    fn server_rate_limit_and_400_retry() {
        for code in [400, 429, 500, 502, 503] {
            assert!(is_retriable_llm_error(&http_err(code, "retry")), "{code} must retry");
        }
    }

    #[test]
    fn non_http_errors_retry() {
        assert!(is_retriable_llm_error(&anyhow::anyhow!("connection reset by peer")));
    }

    #[test]
    fn status_digits_in_the_message_do_not_mislead() {
        // Regression for B6: the old substring check read any "404"/"401" in the text
        // as a client error. A 500 whose body mentions "1401 tokens" / "code 404" must
        // still retry — classification keys on the structured status, not the string.
        let e = http_err(500, "provider error: too many (1401) tokens, see code 404 in docs");
        assert!(is_retriable_llm_error(&e));
    }
}
