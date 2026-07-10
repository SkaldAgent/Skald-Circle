//! Honcho memory plugin — streams completed chat turns to a Honcho server
//! and exposes a [`Memory`] read path via [`HonchoMemory`].
//!
//! # Write path
//! Subscribes to the [`ChatEventBus`] and forwards every user/assistant message
//! from **interactive, non-ephemeral** sessions to Honcho so that the server can
//! build long-term memory (conclusions) about the user.
//!
//! # Read path
//! [`HonchoMemory`] implements the [`Memory`] trait.  Before each LLM turn,
//! `query_context` calls Honcho's `session_context` API to retrieve a
//! token-budgeted summary of what is known so far and injects it into the
//! system prompt.
//!
//! # Filtering (write path)
//! An event is forwarded only when **all** of the following hold:
//! - `is_interactive = true`  — a real user is in the conversation
//! - `is_ephemeral   = false` — not a short-lived automated session (cron, tic)
//! - `is_synthetic   = false` — message content was typed by a user, not
//!                              injected by the system
//!
//! # Honcho object model
//! ```
//! workspace (one per agent instance, from config)
//! ├── peer  "user"      (observe_others = true)
//! ├── peer  "assistant" (observe_me     = true)
//! └── session           (one per local chat_sessions.id, created lazily)
//!     ├── message  peer_id="user"
//!     └── message  peer_id="assistant"
//! ```
//!
//! The `session_map` (local session_id → Honcho session UUID) is shared between
//! the write-path listener task and `HonchoMemory` so both sides see the same
//! mapping without duplication.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::Result;
use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::sync::{Mutex, RwLock};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, trace, warn};

use core_api::bus::{BusEvent, ChatEvent, ChatEventRole, RecvError};
use core_api::memory::Memory;
use core_api::plugin::PluginContext;
use core_api::tool::{Tool, ToolCategory};
use honcho_client::HonchoClient;
use honcho_client::models::{
    ConclusionCreate, MessageCreate, PeerCreate, PeerRepresentationGet,
    SessionCreate, SessionPeerConfig, WorkspaceCreate,
};

const PLUGIN_ID: &str = "honcho";
const PEER_USER: &str = "user";
const PEER_ASSISTANT: &str = "assistant";
/// Token budget for session_context queries.
const CONTEXT_TOKENS: u32 = 2000;

// ── Config ────────────────────────────────────────────────────────────────────

#[derive(Clone, PartialEq)]
struct HonchoConfig {
    base_url:     String,
    api_key:      String,
    workspace_id: String,
}

// ── HonchoMemory ──────────────────────────────────────────────────────────────

/// Implements the [`Memory`] trait for Honcho.
///
/// Created once in [`HonchoPlugin::new`] and shared for the plugin's lifetime.
/// The plugin calls [`HonchoMemory::activate`] on start and
/// [`HonchoMemory::deactivate`] on stop to swap the live client in/out without
/// replacing the `Arc`.
pub struct HonchoMemory {
    /// Mirrors `HonchoPlugin::running`; false when the plugin is stopped.
    running:      Arc<AtomicBool>,
    /// Active client + workspace_id; None when the plugin is not running.
    inner:        std::sync::RwLock<Option<HonchoInner>>,
    /// Shared with the write-path listener task.
    session_map:  Arc<RwLock<HashMap<i64, String>>>,
}

#[derive(Clone)]
struct HonchoInner {
    client:       Arc<HonchoClient>,
    workspace_id: String,
}

impl HonchoMemory {
    fn new(running: Arc<AtomicBool>) -> Self {
        Self {
            running,
            inner:       std::sync::RwLock::new(None),
            session_map: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    fn activate(&self, client: Arc<HonchoClient>, workspace_id: String) {
        *self.inner.write().unwrap() = Some(HonchoInner { client, workspace_id });
    }

    fn deactivate(&self) {
        *self.inner.write().unwrap() = None;
        // Clear the session map so a fresh start builds a clean mapping.
        // (The Honcho sessions themselves are not deleted — they keep accumulating.)
        // Use try_write: if somehow a query is in flight we skip the clear and
        // it will be corrected on the next restart anyway.
        if let Ok(mut map) = self.session_map.try_write() {
            map.clear();
        }
    }

    fn inner(&self) -> Option<HonchoInner> {
        self.inner.read().unwrap().clone()
    }
}

#[async_trait]
impl Memory for HonchoMemory {
    fn id(&self) -> &str { PLUGIN_ID }

    fn is_available(&self) -> bool {
        self.running.load(Ordering::Relaxed)
            && self.inner.read().unwrap().is_some()
    }

    async fn query_context(&self, session_id: i64, user_message: &str) -> Option<String> {
        // Truncate to at most 120 *characters* (not bytes) to avoid a panic on
        // multi-byte UTF-8 codepoints (e.g. 'è' spans two bytes, so a fixed
        // byte-index like 120 can land in the middle of it).
        let preview_end = user_message
            .char_indices()
            .nth(120)          // byte offset of the 121st char = end of first 120 chars
            .map(|(i, _)| i)
            .unwrap_or(user_message.len());
        trace!(
            session_id,
            msg_preview = &user_message[..preview_end],
            "honcho: query_context invoked"
        );

        let HonchoInner { client, workspace_id } = self.inner()?;

        // ── Strategy: peer_context (global) + session_context (current session) ──
        //
        // peer_context with search_query searches conclusions derived from ALL past
        // sessions — this is the only way cross-session references ("remember when
        // we talked about X last week?") can be resolved automatically.
        //
        // session_context is kept as a secondary call for the current session only,
        // to surface conclusions/summaries specific to the ongoing conversation that
        // may not yet be reflected in the peer-level representation.
        //
        // Two embeddings per turn is the cost; the benefit is that the LLM always
        // has both global long-term memory AND current-session context.
        //
        // NOTE: session_context is skipped on the first turn (404 — session not yet
        // created in Honcho by the write path) to avoid a wasted HTTP round-trip.

        // ── 1. Global peer context (cross-session, semantic search) ──────────────
        trace!(session_id, "honcho: querying peer_context (global, with search_query)");
        let peer_ctx = match client.peer_context(
            &workspace_id,
            PEER_USER,
            &PeerRepresentationGet {
                search_query: Some(user_message.to_string()),
                ..Default::default()
            },
        ).await {
            Ok(ctx) => {
                trace!(session_id, raw_json = %ctx, "honcho: peer_context raw response");
                let f = format_context(ctx);
                debug!(
                    "honcho: peer_context (global) for session {session_id} ({} chars)",
                    f.as_deref().map_or(0, |s| s.len())
                );
                f
            }
            Err(e) => {
                warn!("honcho: peer_context failed: {e}");
                None
            }
        };

        // ── 2. Current-session context (session-scoped, no extra embedding) ──────
        //
        // session_context is a GET with search_query but Honcho re-uses the same
        // embedding vector already computed for the peer_context call above
        // (server-side caching).  No additional LM Studio call in practice.
        let deterministic_id = format!("{workspace_id}-{session_id}");
        trace!(session_id, honcho_session_id = %deterministic_id, "honcho: querying session_context");
        let session_ctx = match client.session_context(
            &workspace_id,
            &deterministic_id,
            Some(CONTEXT_TOKENS),
            Some(user_message),
        ).await {
            Ok(ctx) => {
                trace!(session_id, raw_json = %ctx, "honcho: session_context raw response");
                let f = format_context(ctx);
                debug!(
                    "honcho: session_context for session {session_id} ({} chars)",
                    f.as_deref().map_or(0, |s| s.len())
                );
                f
            }
            Err(honcho_client::error::HonchoError::Http { status: 404, .. }) => {
                debug!("honcho: session {deterministic_id} not yet in Honcho (first turn) — skipping session_context");
                None
            }
            Err(e) => {
                warn!("honcho: session_context failed for session {session_id}: {e}");
                None
            }
        };

        // ── 3. Merge: peer (global) first, then session-specific ─────────────────
        let merged = match (peer_ctx, session_ctx) {
            (Some(p), Some(s)) if p != s => {
                trace!(session_id, "honcho: merging peer + session context");
                Some(format!("{p}\n\n{s}"))
            }
            (Some(p), _) => Some(p),
            (_, Some(s)) => Some(s),
            (None, None)  => None,
        };

        if let Some(ref text) = merged {
            trace!(session_id, injected = %text, "honcho: context injected into system prompt");
        } else {
            trace!(session_id, "honcho: no context to inject");
        }

        merged
    }

    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        match self.inner() {
            Some(HonchoInner { client, workspace_id }) => vec![
                Arc::new(MemoryQueryTool {
                    client:       Arc::clone(&client),
                    workspace_id: workspace_id.clone(),
                }),
                Arc::new(HonchoProfileTool {
                    client:       Arc::clone(&client),
                    workspace_id: workspace_id.clone(),
                }),
                Arc::new(HonchoSearchTool {
                    client:       Arc::clone(&client),
                    workspace_id: workspace_id.clone(),
                }),
                Arc::new(HonchoContextTool {
                    client:       Arc::clone(&client),
                    workspace_id: workspace_id.clone(),
                }),
                Arc::new(HonchoConcludeTool { client, workspace_id }),
            ],
            None => vec![],
        }
    }
}

// ── MemoryQueryTool ───────────────────────────────────────────────────────────

/// LLM-callable tool that queries Honcho's Dialectic API.
///
/// The official Honcho documentation explicitly recommends exposing `peer.chat()`
/// as a tool for agents: the LLM decides on its own when extra memory context
/// is needed and calls this tool with a natural-language question.
///
/// Uses `tokio::task::block_in_place` to bridge the sync `Tool::execute` interface
/// with the async HTTP call, safely running inside the existing Tokio runtime.
struct MemoryQueryTool {
    client:       Arc<HonchoClient>,
    workspace_id: String,
}

impl Tool for MemoryQueryTool {
    fn name(&self) -> &str { "memory_query" }

    fn description(&self) -> &str {
        "Query long-term memory about the user using natural language. \
         Ask anything about the user's preferences, past conversations, \
         or known facts. Returns a synthesized answer from Honcho's memory. \
         Use when you need specific information about the user that is not \
         already present in the current conversation."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type":        "string",
                    "description": "Natural language question about the user. \
                                    E.g. 'What programming languages does the user prefer?'"
                }
            },
            "required": ["query"]
        })
    }

    fn category(&self) -> ToolCategory {
        ToolCategory::Introspection
    }

    fn execute(&self, args: Value) -> anyhow::Result<String> {
        let query = args["query"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("memory_query: missing 'query' argument"))?
            .to_string();

        let client       = Arc::clone(&self.client);
        let workspace_id = self.workspace_id.clone();

        // Bridge sync Tool::execute → async HTTP call.
        // block_in_place yields the thread to the Tokio scheduler while the
        // nested block_on drives the future to completion — safe inside an
        // existing multi-thread Tokio runtime.
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async move {
                let opts = honcho_client::models::DialecticOptions {
                    query,
                    session_id:      None,
                    target:          None,
                    stream:          Some(false),
                    reasoning_level: Some("low".to_string()),
                };
                let response = client
                    .peer_chat(&workspace_id, PEER_USER, &opts)
                    .await
                    .map_err(|e| anyhow::anyhow!("memory_query: {e}"))?;

                // The Dialectic endpoint returns a JSON object.
                // Try known content fields; fall back to pretty-printed JSON.
                let text = response.get("content")
                    .or_else(|| response.get("response"))
                    .or_else(|| response.get("message"))
                    .and_then(|v| v.as_str())
                    .map(str::to_string)
                    .unwrap_or_else(|| {
                        serde_json::to_string_pretty(&response)
                            .unwrap_or_else(|_| response.to_string())
                    });

                Ok(text)
            })
        })
    }
}

/// Bridge a synchronous `Tool::execute` to an async Honcho call.
///
/// `block_in_place` yields the worker thread back to the Tokio scheduler while
/// the nested `block_on` drives the future to completion — safe inside the
/// existing multi-thread runtime without spawning a new thread. Shared by all
/// Honcho tools.
fn run_blocking<F, T>(fut: F) -> T
where
    F: std::future::Future<Output = T>,
{
    tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(fut))
}

// ── HonchoProfileTool ─────────────────────────────────────────────────────────

/// Reads or overwrites the user's *peer card* — a curated list of key facts
/// (name, role, preferences, communication style) maintained by Honcho.
struct HonchoProfileTool {
    client:       Arc<HonchoClient>,
    workspace_id: String,
}

impl Tool for HonchoProfileTool {
    fn name(&self) -> &str { "honcho_profile" }

    fn description(&self) -> &str {
        "Read or update the peer card for the user in Honcho — a curated list of \
         key facts (name, role, preferences, communication style). Omit `card` to \
         read the current card; pass `card` as a list of fact strings to overwrite it."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "card": {
                    "type":        "array",
                    "items":       { "type": "string" },
                    "description": "New peer card as a list of fact strings. \
                                    Omit to read the current card."
                }
            }
        })
    }

    fn category(&self) -> ToolCategory { ToolCategory::Introspection }

    fn execute(&self, args: Value) -> anyhow::Result<String> {
        let client       = Arc::clone(&self.client);
        let workspace_id = self.workspace_id.clone();
        let card_update  = args.get("card").and_then(|v| v.as_array()).cloned();

        run_blocking(async move {
            match card_update {
                Some(facts) => {
                    client
                        .set_peer_card(&workspace_id, PEER_USER, None, json!(facts))
                        .await
                        .map_err(|e| anyhow::anyhow!("honcho_profile: {e}"))?;
                    Ok(format!("Peer card updated ({} facts).", facts.len()))
                }
                None => {
                    let card = client
                        .get_peer_card(&workspace_id, PEER_USER, None)
                        .await
                        .map_err(|e| anyhow::anyhow!("honcho_profile: {e}"))?;
                    Ok(serde_json::to_string_pretty(&card)
                        .unwrap_or_else(|_| card.to_string()))
                }
            }
        })
    }
}

// ── HonchoSearchTool ──────────────────────────────────────────────────────────

/// Semantic search over the conclusions Honcho has derived about the user.
/// Returns raw ranked excerpts — no LLM synthesis — including their IDs so the
/// model can later delete a specific one via `honcho_conclude`.
struct HonchoSearchTool {
    client:       Arc<HonchoClient>,
    workspace_id: String,
}

impl Tool for HonchoSearchTool {
    fn name(&self) -> &str { "honcho_search" }

    fn description(&self) -> &str {
        "Semantic search over facts Honcho has derived about the user. Returns raw \
         excerpts ranked by relevance to `query` — no LLM synthesis. Faster and \
         cheaper than memory_query. Each fact includes its id (usable with \
         honcho_conclude) when available."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type":        "string",
                    "description": "What to search for in Honcho's memory about the user."
                }
            },
            "required": ["query"]
        })
    }

    fn category(&self) -> ToolCategory { ToolCategory::Introspection }

    fn execute(&self, args: Value) -> anyhow::Result<String> {
        let query = args["query"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("honcho_search: missing 'query' argument"))?
            .to_string();

        let client       = Arc::clone(&self.client);
        let workspace_id = self.workspace_id.clone();

        // Honcho's `conclusions/query` endpoint requires observer/observed
        // filters; the proven path (shared with the read-path) is `peer_context`
        // with a `search_query`, which ranks the user's conclusions by relevance.
        run_blocking(async move {
            let ctx = client
                .peer_context(
                    &workspace_id,
                    PEER_USER,
                    &PeerRepresentationGet {
                        search_query:  Some(query),
                        search_top_k:  Some(10),
                        ..Default::default()
                    },
                )
                .await
                .map_err(|e| anyhow::anyhow!("honcho_search: {e}"))?;

            Ok(format_conclusions(&ctx)
                .unwrap_or_else(|| "No relevant context found.".to_string()))
        })
    }
}

/// Formats the `conclusions` array of a Honcho `peer_context` response as a
/// ranked bullet list, prefixing each fact with its `id` when present so the
/// model can target it via `honcho_conclude`. Returns `None` when empty.
fn format_conclusions(ctx: &Value) -> Option<String> {
    let conclusions = ctx.get("conclusions")?.as_array()?;
    let lines: Vec<String> = conclusions
        .iter()
        .filter_map(|c| {
            let content = c.get("content").and_then(|v| v.as_str())?;
            match c.get("id").and_then(|v| v.as_str()) {
                Some(id) => Some(format!("- [{id}] {content}")),
                None     => Some(format!("- {content}")),
            }
        })
        .collect();
    (!lines.is_empty()).then(|| lines.join("\n"))
}

// ── HonchoContextTool ─────────────────────────────────────────────────────────

/// Retrieves a full context snapshot for the user (conclusions, card, summary)
/// from Honcho's `peer_context` endpoint. No LLM synthesis.
struct HonchoContextTool {
    client:       Arc<HonchoClient>,
    workspace_id: String,
}

impl Tool for HonchoContextTool {
    fn name(&self) -> &str { "honcho_context" }

    fn description(&self) -> &str {
        "Retrieve a full context snapshot for the user from Honcho — conclusions, \
         peer card, and conversation summary. No LLM synthesis (cheaper than \
         memory_query). Pass an optional `query` to focus the semantic search."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type":        "string",
                    "description": "Optional focus query to filter context. \
                                    Omit for a full context snapshot."
                }
            }
        })
    }

    fn category(&self) -> ToolCategory { ToolCategory::Introspection }

    fn execute(&self, args: Value) -> anyhow::Result<String> {
        let client       = Arc::clone(&self.client);
        let workspace_id = self.workspace_id.clone();
        let search_query = args.get("query").and_then(|v| v.as_str()).map(str::to_string);

        run_blocking(async move {
            let ctx = client
                .peer_context(
                    &workspace_id,
                    PEER_USER,
                    &PeerRepresentationGet { search_query, ..Default::default() },
                )
                .await
                .map_err(|e| anyhow::anyhow!("honcho_context: {e}"))?;

            Ok(format_context(ctx).unwrap_or_else(|| "No context available yet.".to_string()))
        })
    }
}

// ── HonchoConcludeTool ────────────────────────────────────────────────────────

/// Writes or deletes a persistent fact (conclusion) about the user in Honcho's
/// memory. Exactly one of `conclusion` or `delete_id` must be supplied.
///
/// Written as `observer = user`, `observed = user` — matching this plugin's peer
/// model, where the `user` peer has `observe_me = true` and therefore holds the
/// self-knowledge that the read-path (`peer_context("user")`) reads back. Using
/// any other observer slot would store facts the read-path never sees.
struct HonchoConcludeTool {
    client:       Arc<HonchoClient>,
    workspace_id: String,
}

impl Tool for HonchoConcludeTool {
    fn name(&self) -> &str { "honcho_conclude" }

    fn description(&self) -> &str {
        "Write or delete a persistent fact about the user in Honcho's memory. \
         Pass `conclusion` to create a new fact; pass `delete_id` (from \
         honcho_search) to remove one. Exactly one field is required."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "conclusion": {
                    "type":        "string",
                    "description": "A factual statement about the user to persist."
                },
                "delete_id": {
                    "type":        "string",
                    "description": "Conclusion id to delete (e.g. for PII removal)."
                }
            }
        })
    }

    fn category(&self) -> ToolCategory { ToolCategory::Introspection }

    fn execute(&self, args: Value) -> anyhow::Result<String> {
        let conclusion = args.get("conclusion").and_then(|v| v.as_str())
            .map(str::trim).filter(|s| !s.is_empty()).map(str::to_string);
        let delete_id = args.get("delete_id").and_then(|v| v.as_str())
            .map(str::trim).filter(|s| !s.is_empty()).map(str::to_string);

        // Exactly one must be present (XOR).
        if conclusion.is_some() == delete_id.is_some() {
            anyhow::bail!("honcho_conclude: provide exactly one of 'conclusion' or 'delete_id'");
        }

        let client       = Arc::clone(&self.client);
        let workspace_id = self.workspace_id.clone();

        run_blocking(async move {
            if let Some(id) = delete_id {
                client
                    .delete_conclusion(&workspace_id, &id)
                    .await
                    .map_err(|e| anyhow::anyhow!("honcho_conclude: {e}"))?;
                Ok(format!("Conclusion {id} deleted."))
            } else {
                let content = conclusion.unwrap();
                client
                    .add_conclusion(
                        &workspace_id,
                        ConclusionCreate {
                            content:     content.clone(),
                            observer_id: PEER_USER.to_string(),
                            observed_id: PEER_USER.to_string(),
                            session_id:  None,
                        },
                    )
                    .await
                    .map_err(|e| anyhow::anyhow!("honcho_conclude: {e}"))?;
                Ok(format!("Conclusion saved: {content}"))
            }
        })
    }
}

/// Extracts a human-readable string from the raw Honcho `session_context` /
/// `peer_context` JSON response.
///
/// Returns `None` if there is nothing *new* to inject — i.e. when the response
/// contains only raw messages (which are already present in the LLM's own
/// conversation history) or is otherwise empty.
///
/// Only synthesised knowledge is injected:
/// - `conclusions` — facts about the user derived by Honcho's background processing
/// - `summary`     — a narrative summary produced by Honcho
///
/// Raw `messages` are intentionally ignored: they are redundant with the local
/// `chat_history` already sent to the LLM and would waste context tokens.
fn format_context(ctx: Value) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();

    if let Some(conclusions) = ctx.get("conclusions").and_then(|v| v.as_array()) {
        let facts: Vec<&str> = conclusions
            .iter()
            .filter_map(|c| c.get("content").and_then(|v| v.as_str()))
            .collect();
        if !facts.is_empty() {
            parts.push(format!("Known facts about the user:\n- {}", facts.join("\n- ")));
        }
    }

    if let Some(summary) = ctx.get("summary").and_then(|v| v.as_str()) {
        if !summary.trim().is_empty() {
            parts.push(format!("Conversation summary:\n{summary}"));
        }
    }

    if parts.is_empty() {
        return None;
    }

    Some(format!(
        "--- Honcho memory context ---\n{}\n--- end of memory context ---",
        parts.join("\n\n")
    ))
}

// ── HonchoPlugin ──────────────────────────────────────────────────────────────

pub struct HonchoPlugin {
    config:        Mutex<Option<HonchoConfig>>,
    running:       Arc<AtomicBool>,
    cancel:        Mutex<Option<CancellationToken>>,
    handle:        Mutex<Option<JoinHandle<()>>>,
    /// Shared Memory implementation — created once, updated on start/stop.
    honcho_memory: Arc<HonchoMemory>,
}

impl HonchoPlugin {
    pub fn new() -> Self {
        let running = Arc::new(AtomicBool::new(false));
        let honcho_memory = Arc::new(HonchoMemory::new(Arc::clone(&running)));
        Self {
            config:        Mutex::new(None),
            running,
            cancel:        Mutex::new(None),
            handle:        Mutex::new(None),
            honcho_memory,
        }
    }
}

// ── Plugin trait ──────────────────────────────────────────────────────────────

#[async_trait]
impl core_api::plugin::Plugin for HonchoPlugin {
    fn id(&self)          -> &str { PLUGIN_ID }
    fn name(&self)        -> &str { "Honcho Memory" }
    fn description(&self) -> &str {
        "Streams completed interactive chat turns to Honcho for long-term memory \
         and injects retrieved context into every LLM turn."
    }
    fn is_running(&self) -> bool { self.running.load(Ordering::Relaxed) }

    fn memory(&self) -> Option<Arc<dyn Memory>> {
        Some(Arc::clone(&self.honcho_memory) as Arc<dyn Memory>)
    }

    fn config_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "base_url": {
                    "type":        "string",
                    "title":       "Base URL",
                    "description": "Honcho server URL (e.g. http://localhost:8000)",
                    "default":     "http://localhost:8000"
                },
                "api_key": {
                    "type":        "string",
                    "title":       "API Key",
                    "description": "Honcho API key (leave empty for local/unauthenticated instances)",
                    "sensitive":   true
                },
                "workspace_id": {
                    "type":        "string",
                    "title":       "Workspace ID",
                    "description": "Honcho workspace identifier for this agent instance",
                    "default":     "personal-agent"
                }
            },
            "required": ["base_url", "workspace_id"]
        })
    }

    fn as_any(&self) -> &dyn std::any::Any { self }
    fn as_arc_any(self: Arc<Self>) -> Arc<dyn std::any::Any + Send + Sync> { self }

    async fn reload(&self, enabled: bool, config: Value, ctx: PluginContext) -> Result<()> {
        let new_cfg = HonchoConfig {
            base_url:     config["base_url"].as_str().unwrap_or("http://localhost:8000").to_string(),
            api_key:      config["api_key"].as_str().unwrap_or("").to_string(),
            workspace_id: config["workspace_id"].as_str().unwrap_or("personal-agent").to_string(),
        };

        let old_cfg     = self.config.lock().await.clone();
        let is_running  = self.is_running();
        let cfg_changed = old_cfg.as_ref().map_or(true, |old| old != &new_cfg);

        match (enabled, is_running) {
            (true, false) => {
                anyhow::ensure!(
                    !new_cfg.base_url.is_empty(),
                    "honcho: cannot start — `base_url` is missing from config"
                );
                *self.config.lock().await = Some(new_cfg);
                self.start(ctx).await?;
            }
            (false, true) => {
                self.stop().await?;
                *self.config.lock().await = None;
            }
            (true, true) if cfg_changed => {
                info!("honcho: config changed — restarting");
                self.stop().await?;
                *self.config.lock().await = Some(new_cfg);
                self.start(ctx).await?;
            }
            _ => {}
        }
        Ok(())
    }

    async fn start(&self, ctx: PluginContext) -> Result<()> {
        if self.running.load(Ordering::Relaxed) {
            return Ok(());
        }

        let cfg = self.config.lock().await.clone()
            .ok_or_else(|| anyhow::anyhow!("honcho: config not set"))?;

        let client       = Arc::new(HonchoClient::with_base_url(&cfg.base_url, &cfg.api_key));
        let workspace_id = cfg.workspace_id.clone();

        self.honcho_memory.activate(Arc::clone(&client), workspace_id.clone());

        let session_map  = Arc::clone(&self.honcho_memory.session_map);
        let mut rx       = ctx.event_bus.subscribe();
        let cancel       = CancellationToken::new();
        let cancel_clone = cancel.clone();
        let running      = Arc::clone(&self.running);

        self.running.store(true, Ordering::Relaxed);

        let task = tokio::spawn(async move {
            ensure_workspace_ready(&client, &workspace_id).await;

            info!("honcho plugin: listener started (workspace={workspace_id})");
            loop {
                tokio::select! {
                    _ = cancel_clone.cancelled() => {
                        info!("honcho plugin: cancelled");
                        break;
                    }
                    result = rx.recv() => {
                        match result {
                            Ok(BusEvent::UserMessage(event)) |
                            Ok(BusEvent::AssistantResponse(event)) => {
                                handle_event(
                                    &client, &workspace_id, event, &session_map,
                                ).await;
                            }
                            Ok(BusEvent::CompactionDone(_)) => {}
                            Err(RecvError::Lagged(n)) => {
                                warn!(
                                    "honcho plugin: event bus lagged by {n} events \
                                     — some turns missed"
                                );
                            }
                            Err(RecvError::Closed) => {
                                info!("honcho plugin: event bus closed");
                                break;
                            }
                        }
                    }
                }
            }
            running.store(false, Ordering::Relaxed);
        });

        *self.cancel.lock().await = Some(cancel);
        *self.handle.lock().await = Some(task);
        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        if let Some(token) = self.cancel.lock().await.take() {
            token.cancel();
        }
        if let Some(h) = self.handle.lock().await.take() {
            let _ = h.await;
        }
        self.running.store(false, Ordering::Relaxed);
        self.honcho_memory.deactivate();
        Ok(())
    }
}

// ── Internal helpers ──────────────────────────────────────────────────────────

async fn ensure_workspace_ready(client: &HonchoClient, workspace_id: &str) {
    match client.create_workspace(&WorkspaceCreate {
        id:            workspace_id.to_string(),
        metadata:      None,
        configuration: None,
    }).await {
        Ok(_)  => info!("honcho: workspace '{workspace_id}' ready"),
        Err(e) => warn!("honcho: workspace '{workspace_id}' create/check failed: {e}"),
    }

    for peer_id in [PEER_USER, PEER_ASSISTANT] {
        match client.create_peer(workspace_id, &PeerCreate {
            id:            peer_id.to_string(),
            metadata:      None,
            configuration: None,
        }).await {
            Ok(_)  => debug!("honcho: peer '{peer_id}' ready"),
            Err(e) => debug!("honcho: peer '{peer_id}' create/check: {e} (likely already exists)"),
        }
    }
}

async fn handle_event(
    client:       &HonchoClient,
    workspace_id: &str,
    event:        ChatEvent,
    session_map:  &Arc<RwLock<HashMap<i64, String>>>,
) {
    if !event.is_interactive || event.is_ephemeral || event.is_synthetic {
        return;
    }

    let peer_id = match event.role {
        ChatEventRole::User      => PEER_USER,
        ChatEventRole::Assistant => PEER_ASSISTANT,
        ChatEventRole::Agent     => return,
    };

    if event.content.is_empty() {
        return;
    }

    let honcho_session_id = match get_or_create_session(
        client, workspace_id, event.session_id, session_map,
    ).await {
        Ok(id)  => id,
        Err(e)  => {
            warn!(
                "honcho: failed to get/create session for local session {}: {e}",
                event.session_id
            );
            return;
        }
    };

    let msg = MessageCreate {
        content:       event.content,
        peer_id:       peer_id.to_string(),
        metadata:      Some(json!({
            "local_message_id": event.message_id,
            "local_stack_id":   event.stack_id,
        })),
        configuration: None,
        created_at:    Some(event.created_at.to_rfc3339()),
    };

    match client.add_message(workspace_id, &honcho_session_id, msg).await {
        Ok(_)  => debug!(
            "honcho: message sent (session={honcho_session_id}, peer={peer_id})"
        ),
        Err(e) => warn!(
            "honcho: add_message failed (session={honcho_session_id}): {e}"
        ),
    }
}

async fn get_or_create_session(
    client:           &HonchoClient,
    workspace_id:     &str,
    local_session_id: i64,
    session_map:      &Arc<RwLock<HashMap<i64, String>>>,
) -> Result<String> {
    {
        let map = session_map.read().await;
        if let Some(id) = map.get(&local_session_id) {
            return Ok(id.clone());
        }
    }

    let mut peers = HashMap::new();
    peers.insert(PEER_USER.to_string(), SessionPeerConfig {
        observe_others: None,
        observe_me:     Some(true),
    });
    peers.insert(PEER_ASSISTANT.to_string(), SessionPeerConfig {
        observe_me:     Some(true),
        observe_others: None,
    });

    // Use a deterministic id so the mapping survives plugin restarts without
    // needing a DB column — same local_session_id always maps to the same
    // Honcho session. Honcho v3 requires `id` in the creation body.
    let honcho_id = format!("{workspace_id}-{local_session_id}");

    let session = client.create_session(workspace_id, &SessionCreate {
        id:            Some(honcho_id),
        metadata:      Some(json!({ "local_session_id": local_session_id })),
        peers:         Some(peers),
        configuration: None,
    }).await?;

    info!(
        "honcho: created session {} for local session {local_session_id}",
        session.id
    );

    let mut map = session_map.write().await;
    map.entry(local_session_id).or_insert(session.id.clone());
    Ok(map[&local_session_id].clone())
}
