use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;

use axum::{
    Json, Extension,
    extract::{Path, Query, State},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::SqlitePool;

use skald_core::db::{chat_history, chat_llm_tools, chat_sessions, chat_sessions_stack, roles, sources};
use skald_core::db::chat_sessions_stack::SessionStack;
use skald_core::run_context::RunContext;
use std::sync::Arc;
use skald_core::skald::{Skald, UserContext};
use skald_core::session::handler::ApprovalDecision;
use skald_core::approval::ApprovalManager;
use skald_core::tools::{ToolRegistry, ToolDescriptionLength, tool_names as tn};

use super::{ApiError, guard::AuthUser, require_context};

// ── POST /api/sessions — start a new conversation ─────────────────────────────

#[derive(Deserialize)]
pub struct CreateQuery {
    #[serde(default = "default_source")]
    pub source: String,
}

fn default_source() -> String { "web".to_string() }

pub async fn create(
    State(skald): State<Arc<Skald>>,
    Extension(auth): Extension<AuthUser>,
    Query(q): Query<CreateQuery>,
) -> Result<Json<Value>, ApiError> {
    let ctx = require_context(&skald, &auth.user_id).await?;
    // Resolve agent + RunContext from the source so project chats reset with the
    // coordinator agent (not the default `main`), then provision a fresh session.
    let (agent, rc) = super::projects::provisioning_for_source(&ctx.pool, &q.source).await?;
    // A non-project chat inherits the caller role's default security-group, so a
    // restricted role starts scoped instead of on the catch-all `default` group.
    // Project chats already carry their own run-context and are left untouched.
    let rc = match rc {
        Some(rc) => Some(rc),
        None => role_default_run_context(&skald, &auth.user_id).await?,
    };
    ctx.chat_hub.provision_session(&q.source, &agent, rc.as_ref(), true).await?;
    Ok(Json(json!({})))
}

/// The default security-group a new session gets from the owner's role, or `None`
/// when the role points at the catch-all `default` group (nothing to pin).
async fn role_default_run_context(
    skald:   &Skald,
    user_id: &str,
) -> Result<Option<RunContext>, ApiError> {
    let Some(user) = skald.users().get(user_id).await? else { return Ok(None) };
    let Some(role) = roles::get(skald.db(), &user.role_id).await? else { return Ok(None) };
    let group = role.permission_group;
    if group.is_empty() || group == "default" {
        return Ok(None);
    }
    Ok(Some(RunContext::with_security_group(Some(group))))
}

// ── GET /api/web/messages ─────────────────────────────────────────────────────

pub async fn web_messages(
    State(skald): State<Arc<Skald>>,
    Extension(auth): Extension<AuthUser>,
) -> Result<Json<Vec<Value>>, ApiError> {
    let ctx = require_context(&skald, &auth.user_id).await?;
    messages_for_source(&skald, &ctx, "web").await
}

// ── GET /api/:source/messages ─────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct SourcePath { pub source: String }

pub async fn source_messages(
    State(skald): State<Arc<Skald>>,
    Extension(auth): Extension<AuthUser>,
    Path(p): Path<SourcePath>,
) -> Result<Json<Vec<Value>>, ApiError> {
    let ctx = require_context(&skald, &auth.user_id).await?;
    messages_for_source(&skald, &ctx, &p.source).await
}

async fn messages_for_source(skald: &Arc<Skald>, ctx: &UserContext, source: &str) -> Result<Json<Vec<Value>>, ApiError> {
    // History/sessions read from the caller's own pool; the tool registry is a
    // global capability, and approval is this user's per-user manager.
    let db = &ctx.pool;
    let session_id = match sources::active_session_id(db, source).await? {
        Some(id) => id,
        None     => return Ok(Json(vec![])),
    };

    let main_stack = match chat_sessions_stack::main_for_session(db, session_id).await? {
        Some(s) => s,
        None    => return Ok(Json(vec![])),
    };

    let subagent_map: HashMap<i64, SessionStack> =
        chat_sessions_stack::all_for_session(db, session_id)
            .await?
            .into_iter()
            .filter_map(|s| s.parent_tool_call_id.map(|tc_id| (tc_id, s)))
            .collect();

    let mut items: Vec<Value> = Vec::new();
    build_items(db, skald.tools(), &ctx.approval, &main_stack, &subagent_map, &mut items).await?;

    Ok(Json(items))
}

// ── POST /api/tools/:tool_call_id/resolve — approve/reject a pending tool ─────
// (source-agnostic; /api/web/tools/... kept as a back-compat alias)

#[derive(Deserialize)]
pub struct ResolveToolPath {
    pub tool_call_id: i64,
}

#[derive(Deserialize)]
pub struct ResolveToolBody {
    /// `"approve"` or `"reject"`
    pub action: String,
    #[serde(default)]
    pub note: String,
}

#[derive(Serialize)]
pub struct ResolveToolResponse {
    pub tool_call_id: i64,
    pub status:       String,
    pub result:       Option<String>,
    /// Result type tag (`"string"` | `"json"`) so the frontend keeps rich JSON
    /// rendering after resolving an approval, matching the live `ToolDone` event.
    pub result_type:  String,
}

/// Approve or reject a `pending` tool call, resolved by its globally-unique
/// `tool_call_id`. Source-agnostic: the owning session is derived from the tool
/// call's own stack row, so approvals from any client (web/mobile/telegram/cron)
/// resolve against the correct session — there is no "current session" scoping.
pub async fn resolve_tool(
    State(skald): State<Arc<Skald>>,
    Extension(auth): Extension<AuthUser>,
    Path(p): Path<ResolveToolPath>,
    Json(body): Json<ResolveToolBody>,
) -> Result<Json<ResolveToolResponse>, ApiError> {
    let ctx = require_context(&skald, &auth.user_id).await?;
    let db = &ctx.pool;
    // Look up the tool call by id alone — no active-session filter. Also pull the
    // owning session_id so the post-restart path drives the correct session.
    let tc = sqlx::query_as::<_, (i64, String, Option<String>, String, i64)>(
        "SELECT t.id, t.name, t.arguments, t.status, ss.session_id
         FROM   chat_llm_tools t
         JOIN   chat_history h ON h.id = t.message_id
         JOIN   chat_sessions_stack ss ON ss.id = h.session_stack_id
         WHERE  t.id = ?",
    )
    .bind(p.tool_call_id)
    .fetch_optional(&**db)
    .await?
    .ok_or_else(|| anyhow::anyhow!(
        "tool_call_id {} not found", p.tool_call_id
    ))?;

    let (tc_id, tc_name, tc_args_raw, tc_status, session_id) = tc;

    if tc_status != "pending" {
        return Err(anyhow::anyhow!(
            "tool_call {} is not pending (status: {})", tc_id, tc_status
        ).into());
    }

    let args: Value = tc_args_raw.as_deref()
        .and_then(|s| serde_json::from_str(s).ok())
        .unwrap_or(Value::Object(Default::default()));

    if body.action == "reject" {
        // Pass the raw user note to the live session so the loop builds the
        // canonical message; for the not-live path (no waiting session, e.g.
        // after a restart) build the same message here and save it directly.
        let live = ctx.approval
            .resolve_for_tool_call(tc_id, ApprovalDecision::Rejected { note: body.note.clone() })
            .await;
        let msg = ApprovalDecision::rejection_message(&body.note);
        if !live {
            chat_llm_tools::reject(db, tc_id, &msg).await?;
        }
        return Ok(Json(ResolveToolResponse {
            tool_call_id: tc_id,
            status:       "rejected".to_string(),
            result:       Some(msg),
            result_type:  "string".to_string(),
        }));
    }

    // `restart` calls process::exit — mark done in DB first.
    if tc_name == tn::RESTART {
        chat_llm_tools::complete(db, tc_id, "Riavvio avviato.", "string").await?;
        // Use _exit() to skip C atexit handlers (e.g. Metal GPU cleanup in
        // whisper-rs/ggml, which aborts with SIGABRT and yields exit code 134
        // instead of 255 — breaking the run.sh restart supervisor).
        unsafe { libc::_exit(-1) }
    }

    // ── Live path: LLM loop is blocked waiting for approval ──────────────────
    if ctx.approval
        .resolve_for_tool_call(tc_id, ApprovalDecision::Approved)
        .await
    {
        return Ok(Json(ResolveToolResponse {
            tool_call_id: tc_id,
            status:       "running".to_string(),
            result:       None,
            result_type:  "string".to_string(),
        }));
    }

    // ── Post-restart path: no in-memory oneshot to unblock. ───────────────────
    // Sub-agent tools (`execute_task` etc.) cannot run through the flat
    // `execute_tool` path — they need the recursive dispatcher. Mark the call
    // pre-approved and drive the owning session's resume, which re-dispatches it
    // via `execute_tool_call` (gate skipped) and continues the loop. Events stream
    // to the reconnected client through the global bus; return immediately.
    if tc_name == "execute_task" || tc_name == tn::EXECUTE_SUBTASK || tc_name == "run_subtask" {
        let handler = ctx.chat_hub.handler_for_session(session_id).await?;
        handler.mark_pre_approved(tc_id);
        let hub = ctx.chat_hub.clone();
        tokio::spawn(async move {
            if let Err(e) = hub.resume_session(session_id).await {
                tracing::warn!(session_id, tool_call_id = tc_id, error = %e, "post-restart resume of sub-agent tool failed");
            }
        });
        return Ok(Json(ResolveToolResponse {
            tool_call_id: tc_id,
            status:       "running".to_string(),
            result:       None,
            result_type:  "string".to_string(),
        }));
    }

    // Simple tools: execute directly on the owning session and return the result.
    let handler = ctx.chat_hub.handler_for_session(session_id).await?;
    match handler.execute_tool(&tc_name, args).await {
        Ok(result) => {
            let wire = result.to_wire();
            let kind = result.kind();
            chat_llm_tools::complete(db, tc_id, &wire, kind).await?;
            Ok(Json(ResolveToolResponse {
                tool_call_id: tc_id,
                status:       "done".to_string(),
                result:       Some(wire),
                result_type:  kind.to_string(),
            }))
        }
        Err(e) => {
            let msg = e.to_string();
            chat_llm_tools::fail(db, tc_id, &msg).await?;
            Err(anyhow::anyhow!(msg).into())
        }
    }
}

// ── GET /api/sessions — list sessions by source (paginated) ──────────────────

#[derive(Deserialize)]
pub struct ListSessionsQuery {
    pub source:   Option<String>,
    #[serde(default = "default_page")]
    pub page:     i64,
    #[serde(default = "default_per_page")]
    pub per_page: i64,
}

fn default_page()     -> i64 { 1 }
fn default_per_page() -> i64 { 20 }

pub async fn list_sessions(
    State(skald): State<Arc<Skald>>,
    Extension(auth): Extension<AuthUser>,
    Query(q): Query<ListSessionsQuery>,
) -> Result<Json<Value>, ApiError> {
    let ctx = require_context(&skald, &auth.user_id).await?;
    let db = &ctx.pool;
    let per_page = q.per_page.max(1).min(100);
    let offset   = ((q.page.max(1)) - 1) * per_page;
    let src      = q.source.as_deref();

    let total: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM chat_sessions cs
         WHERE (? IS NULL OR cs.source = ?)",
    )
    .bind(src).bind(src)
    .fetch_one(&**db).await?;

    let rows = sqlx::query_as::<_, (i64, String, String, bool, bool, Option<String>, i64, Option<String>)>(
        "SELECT cs.id, cs.source, cs.agent_id, cs.is_ephemeral, cs.is_interactive,
                cs.created_at,
                COUNT(h.id)       AS message_count,
                MAX(h.created_at) AS last_message_at
         FROM   chat_sessions cs
         LEFT   JOIN chat_sessions_stack ss ON ss.session_id = cs.id AND ss.depth = 0
         LEFT   JOIN chat_history h         ON h.session_stack_id = ss.id AND h.status = 'ok'
         WHERE  (? IS NULL OR cs.source = ?)
         GROUP  BY cs.id
         ORDER  BY cs.id DESC
         LIMIT  ? OFFSET ?",
    )
    .bind(src).bind(src)
    .bind(per_page).bind(offset)
    .fetch_all(&**db).await?;

    let items: Vec<Value> = rows.into_iter().map(|(id, source, agent_id, is_ephemeral, is_interactive, created_at, message_count, last_message_at)| {
        json!({
            "id":              id,
            "source":          source,
            "agent_id":        agent_id,
            "is_ephemeral":    is_ephemeral,
            "is_interactive":  is_interactive,
            "created_at":      created_at,
            "message_count":   message_count,
            "last_message_at": last_message_at,
        })
    }).collect();

    Ok(Json(json!({
        "items":    items,
        "total":    total,
        "page":     q.page.max(1),
        "per_page": per_page,
    })))
}

// ── GET /api/sessions/:id — read-only session detail (debug view) ─────────────

#[derive(Deserialize)]
pub struct SessionIdPath { pub id: i64 }

pub async fn get_session_detail(
    State(skald): State<Arc<Skald>>,
    Extension(auth): Extension<AuthUser>,
    Path(p): Path<SessionIdPath>,
) -> Result<Json<Value>, ApiError> {
    let ctx = require_context(&skald, &auth.user_id).await?;
    let db = &ctx.pool;
    let session = chat_sessions::find_by_id(db, p.id)
        .await?
        .ok_or_else(|| ApiError::not_found(format!("session {} not found", p.id)))?;

    let created_at: Option<String> = sqlx::query_scalar(
        "SELECT created_at FROM chat_sessions WHERE id = ?",
    )
    .bind(p.id)
    .fetch_optional(&**db)
    .await?;

    let all_stacks = chat_sessions_stack::all_for_session(db, session.id).await?;

    let subagent_map: HashMap<i64, SessionStack> = all_stacks
        .iter()
        .filter_map(|s| s.parent_tool_call_id.map(|tc_id| (tc_id, s.clone())))
        .collect();

    let main_stack = match all_stacks.into_iter().find(|s| s.depth == 0) {
        Some(s) => s,
        None    => return Ok(Json(json!({
            "session": {
                "id": session.id, "source": session.source,
                "agent_id": session.agent_id, "created_at": created_at,
            },
            "messages": [],
        }))),
    };

    let mut messages: Vec<Value> = Vec::new();
    build_debug_items(db, skald.tools(), &main_stack, &subagent_map, &mut messages).await?;

    Ok(Json(json!({
        "session": {
            "id":             session.id,
            "source":         session.source,
            "agent_id":       session.agent_id,
            "is_interactive": session.is_interactive,
            "is_ephemeral":   session.is_ephemeral,
            "created_at":     created_at,
        },
        "messages": messages,
    })))
}

/// Like `build_items` but includes synthetic user messages and reasoning content.
/// Used exclusively by the session-detail debug view.
fn build_debug_items<'a>(
    db:           &'a SqlitePool,
    tools:        &'a ToolRegistry,
    stack:        &'a SessionStack,
    subagent_map: &'a HashMap<i64, SessionStack>,
    items:        &'a mut Vec<Value>,
) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + 'a>> {
    Box::pin(async move {
        let messages = chat_history::for_stack_all(db, stack.id).await?;

        for msg in &messages {
            let failed = msg.status == "failed";
            match msg.role {
                chat_history::Role::User => {
                    let attachments = msg.metadata.as_ref()
                        .map(|m| m.attachments.clone())
                        .unwrap_or_default();
                    // Custom slash commands render the typed command, not the
                    // expanded template persisted for LLM replay.
                    let content = msg.metadata.as_ref()
                        .and_then(|m| m.command.as_ref())
                        .map(|c| c.display.clone())
                        .unwrap_or_else(|| msg.content.clone());
                    items.push(json!({
                        "kind":         "user",
                        "content":      content,
                        "attachments":  attachments,
                        "failed":       failed,
                        "is_synthetic": msg.is_synthetic,
                        "created_at":   msg.created_at,
                    }));
                }
                chat_history::Role::Agent => {}
                chat_history::Role::Assistant => {
                    let tool_calls = chat_llm_tools::for_message(db, msg.id).await?;
                    if tool_calls.is_empty() {
                        items.push(json!({
                            "kind":          "assistant",
                            "content":       msg.content,
                            "reasoning":     msg.reasoning_content,
                            "failed":        failed,
                            "input_tokens":  msg.input_tokens,
                            "output_tokens": msg.output_tokens,
                            "created_at":    msg.created_at,
                        }));
                    } else {
                        if !msg.content.trim().is_empty() || msg.reasoning_content.is_some() {
                            items.push(json!({
                                "kind":          "thinking",
                                "message_id":    msg.id,
                                "content":       msg.content,
                                "reasoning":     msg.reasoning_content,
                                "failed":        failed,
                                "input_tokens":  msg.input_tokens,
                                "output_tokens": msg.output_tokens,
                                "created_at":    msg.created_at,
                            }));
                        }
                        for tc in &tool_calls {
                            let args: Value = tc.arguments.as_deref()
                                .and_then(|s| serde_json::from_str(s).ok())
                                .unwrap_or(Value::Null);

                            let (status, result, error) = match tc.status.as_str() {
                                "done"      => ("done",      tc.result.clone(), None),
                                "pending"   => ("pending",   None,              None),
                                "running"   => ("error",     None,              Some("Interrupted.".to_string())),
                                "cancelled" => ("cancelled", None,              tc.result.clone()),
                                "rejected"  => ("rejected",  None,              tc.result.clone()),
                                _           => ("error",     None,              tc.result.clone()),
                            };

                            let label_short = tools.describe_call(&tc.name, &args, ToolDescriptionLength::Short);
                            let label_full  = tools.describe_call(&tc.name, &args, ToolDescriptionLength::Full);
                            let target_path = tools.target_path(&tc.name, &args);
                            items.push(json!({
                                "kind":         "tool",
                                "tool_call_id": tc.id,
                                "name":         tc.name,
                                "label_short":  label_short,
                                "label_full":   label_full,
                                "path":         target_path,
                                "arguments":    args,
                                "status":       status,
                                "result":       result,
                                "result_type":  tc.result_type,
                                "error":        error,
                            }));

                            if let Some(sub_stack) = subagent_map.get(&tc.id) {
                                items.push(json!({
                                    "kind":     "agent",
                                    "stack_id": sub_stack.id,
                                    "agent_id": sub_stack.agent_id,
                                    "depth":    sub_stack.depth,
                                    "done":     true,
                                }));
                                build_debug_items(db, tools, sub_stack, subagent_map, items).await?;
                                items.push(json!({
                                    "kind":     "agent_end",
                                    "agent_id": sub_stack.agent_id,
                                    "depth":    sub_stack.depth,
                                }));
                            }
                        }
                    }
                }
            }
        }
        Ok(())
    })
}

// ── Recursive message-tree builder ────────────────────────────────────────────

fn build_items<'a>(
    db:           &'a SqlitePool,
    tools:        &'a ToolRegistry,
    approval:     &'a ApprovalManager,
    stack:        &'a SessionStack,
    subagent_map: &'a HashMap<i64, SessionStack>,
    items:        &'a mut Vec<Value>,
) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + 'a>> {
    Box::pin(async move {
        let messages = chat_history::for_stack_all(db, stack.id).await?;

        for msg in &messages {
            let failed = msg.status == "failed";
            match msg.role {
                chat_history::Role::User => {
                    // Skip synthetic messages (TIC notifications, etc.) — they are
                    // injected as user turns for the LLM but must not appear in the UI.
                    if msg.is_synthetic {
                        continue;
                    }
                    // `content` stays clean (typed text); attachments are surfaced
                    // structurally so the UI renders chips, not the LLM-facing block.
                    let attachments = msg.metadata.as_ref()
                        .map(|m| m.attachments.clone())
                        .unwrap_or_default();
                    // Custom slash commands render the typed command, not the
                    // expanded template persisted for LLM replay.
                    let content = msg.metadata.as_ref()
                        .and_then(|m| m.command.as_ref())
                        .map(|c| c.display.clone())
                        .unwrap_or_else(|| msg.content.clone());
                    items.push(json!({ "kind": "user", "content": content, "attachments": attachments, "failed": failed }));
                }
                chat_history::Role::Agent => {}
                chat_history::Role::Assistant => {
                    let tool_calls = chat_llm_tools::for_message(db, msg.id).await?;
                    if tool_calls.is_empty() {
                        items.push(json!({
                            "kind":          "assistant",
                            "content":       msg.content,
                            "failed":        failed,
                            "input_tokens":  msg.input_tokens,
                            "output_tokens": msg.output_tokens,
                        }));
                    } else {
                        if !msg.content.trim().is_empty() {
                            items.push(json!({
                                "kind":          "thinking",
                                "message_id":    msg.id,
                                "content":       msg.content,
                                "failed":        failed,
                                "input_tokens":  msg.input_tokens,
                                "output_tokens": msg.output_tokens,
                            }));
                        }
                        for tc in &tool_calls {
                            let args: Value = tc.arguments.as_deref()
                                .and_then(|s| serde_json::from_str(s).ok())
                                .unwrap_or(Value::Null);

                            let (status, result, error) = match tc.status.as_str() {
                                "done"    => ("done",    tc.result.clone(), None),
                                // 'pending' means waiting for explicit user input (approval or
                                // clarification) — show the approval form with no error message.
                                "pending" => ("pending", None,              None),
                                // 'running' means the tool was mid-execution when the session was
                                // interrupted — shown as "Interrupted" so the frontend can auto-resume.
                                "running" => ("error",   None,              Some("Interrupted.".to_string())),
                                // 'failed' means the tool completed with a genuine error — show
                                // the actual error message, NOT "Interrupted" (that would trigger
                                // a spurious auto-resume on page refresh).
                                _         => ("error",   None,              tc.result.clone()),
                            };

                            // A pending tool is awaiting approval. Surface the live
                            // `request_id` (only present while the server is up) so the
                            // client resolves via the source-agnostic WS/Inbox path with
                            // bypass support; `null` → the client falls back to resolving
                            // by the durable `tool_call_id`.
                            let request_id = if status == "pending" {
                                approval.request_id_for_tool_call(tc.id).await
                            } else {
                                None
                            };

                            let label_short = tools.describe_call(&tc.name, &args, ToolDescriptionLength::Short);
                            let label_full  = tools.describe_call(&tc.name, &args, ToolDescriptionLength::Full);
                            let target_path = tools.target_path(&tc.name, &args);
                            items.push(json!({
                                "kind":         "tool",
                                "tool_call_id": tc.id,
                                "request_id":   request_id,
                                "name":         tc.name,
                                "label_short":  label_short,
                                "label_full":   label_full,
                                "path":         target_path,
                                "arguments":    args,
                                "status":       status,
                                "result":       result,
                                "result_type":  tc.result_type,
                                "error":        error,
                            }));

                            if let Some(sub_stack) = subagent_map.get(&tc.id) {
                                items.push(json!({
                                    "kind":     "agent",
                                    "stack_id": sub_stack.id,
                                    "agent_id": sub_stack.agent_id,
                                    "depth":    sub_stack.depth,
                                    "done":     true,
                                }));
                                build_items(db, tools, approval, sub_stack, subagent_map, items).await?;
                                items.push(json!({
                                    "kind":     "agent_end",
                                    "agent_id": sub_stack.agent_id,
                                    "depth":    sub_stack.depth,
                                }));
                            }
                        }
                    }
                }
            }
        }
        Ok(())
    })
}
