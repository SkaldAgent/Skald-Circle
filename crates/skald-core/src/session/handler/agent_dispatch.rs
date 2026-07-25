use std::collections::HashSet;
use std::sync::{Arc, RwLock};

use serde_json::Value;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::info;

use crate::db::{activated_tools, chat_history, chat_llm_tools, chat_sessions_stack, scratchpad};
use crate::events::ServerEvent;

use super::{ChatSessionHandler, MAX_AGENT_DEPTH, TurnOutcome};
use super::emitter::TurnEmitter;
use super::interface_tools::{AgentRunConfig, InterfaceTool, ToolFuture};
use super::config::activate_tools_tool_def;

impl ChatSessionHandler {
    /// Dispatches a sub-agent as a child stack frame within the current session.
    /// Used by `execute_task` (mode=sync) and `execute_subtask` interceptions in `llm_loop`.
    /// Args must contain `agent_id` and `prompt`; optionally `client`.
    pub(super) async fn dispatch_sub_agent(
        &self,
        parent_stack_id:     i64,
        parent_config:       &AgentRunConfig,
        parent_tool_call_id: i64,
        args:                &Value,
        token:               &CancellationToken,
        tx:                  &mpsc::Sender<ServerEvent>,
    ) -> anyhow::Result<String> {
        let pool = &self.db;
        let em   = TurnEmitter::new(tx);

        let target_id = args["agent_id"].as_str()
            .ok_or_else(|| anyhow::anyhow!("dispatch_sub_agent: missing required argument `agent_id`"))?;
        let prompt = args["prompt"].as_str()
            .ok_or_else(|| anyhow::anyhow!("dispatch_sub_agent: missing required argument `prompt`"))?;

        if target_id == parent_config.agent_id {
            anyhow::bail!("dispatch_sub_agent: an agent cannot call itself (`{target_id}`)");
        }
        // Only `task` agents are dispatchable: this rejects `chat` (e.g. `main`,
        // `project-coordinator`) and `system` (e.g. `tic`) agents, and surfaces a
        // not-found error for unknown ids — all in one gate.
        let target_meta = crate::agents::load_task_meta(target_id)
            .map_err(|e| anyhow::anyhow!("dispatch_sub_agent: {e}"))?;

        let parent_frame = chat_sessions_stack::find_by_id(pool, parent_stack_id).await?
            .ok_or_else(|| anyhow::anyhow!("dispatch_sub_agent: parent stack frame not found"))?;
        let new_depth = parent_frame.depth + 1;
        if new_depth > MAX_AGENT_DEPTH {
            anyhow::bail!(
                "dispatch_sub_agent: maximum agent depth ({}) exceeded — refusing to recurse further",
                MAX_AGENT_DEPTH
            );
        }

        let explicit_client = args["client"].as_str().or(target_meta.client.as_deref());
        let (resolved_client, _) = self.llm_manager.resolve(
            explicit_client,
            target_meta.strength,
        ).await.map_err(|e| anyhow::anyhow!("dispatch_sub_agent: {e}"))?;

        let child = chat_sessions_stack::create(
            pool,
            self.session_id,
            target_id,
            Some(prompt),
            new_depth,
            Some(parent_tool_call_id),
        ).await?;

        // Single source of the sub-agent's config (base tools + augmentation + grants
        // + activate_tools), shared with restart recovery so the two can't drift (B3).
        let child_config = self.build_sub_agent_config(
            parent_config, target_id, resolved_client.clone(), child.id, new_depth,
        ).await?;

        chat_history::append(pool, child.id, &chat_history::Role::Agent, prompt, false, None).await?;

        let prompt_preview = super::preview_truncate(prompt, 500);

        em.agent_start(
            child.id,
            parent_tool_call_id,
            target_id.to_string(),
            parent_config.agent_id.clone(),
            new_depth,
            prompt_preview,
        ).await;

        info!(
            session_id   = self.session_id,
            parent_stack = parent_stack_id,
            child_stack  = child.id,
            target_agent = target_id,
            client       = %resolved_client,
            "dispatch_sub_agent: running child inline"
        );

        // Run the child synchronously in the SAME task, holding the same
        // `processing` lock and sharing the same cancellation token. The returned
        // string becomes the parent tool call's result, which `run_agent_turn`
        // persists and emits as `ToolDone` — so completion lives in one place.
        // Boxed: `resume_pending_tools` now dispatches sub-agents via `execute_tool_call`,
        // which re-enters here — box this edge so the recursive async future stays sized.
        let _ = Box::pin(self.resume_pending_tools(child.id, &child_config, token, tx)).await;
        // Sub-agents never inject live user input.
        let outcome = self.run_agent_turn(child.id, &child_config, token, tx, None).await;

        if let Err(e) = activated_tools::delete_for_stack(pool, child.id).await {
            tracing::warn!(stack_id = child.id, error = %e, "dispatch_sub_agent: failed to delete stack activations");
        }

        let parent_agent_id = parent_config.agent_id.clone();
        let child_agent_id  = target_id.to_string();
        let preview = |s: &str| super::preview_truncate(s, 500);

        let result = match outcome {
            Ok(TurnOutcome::Final { content, .. }) => {
                em.agent_done(child.id, child_agent_id, parent_agent_id, preview(&content)).await;
                Ok(content)
            }
            Ok(TurnOutcome::Cancelled) => {
                // The parent shares this token: if the cancel came from the user,
                // its next round check returns Cancelled too. We still record a
                // tool result so the history stays well-formed.
                em.agent_done(child.id, child_agent_id, parent_agent_id, "⚠️ Cancelled.".to_string()).await;
                Ok(format!("Sub-agent `{target_id}` was cancelled."))
            }
            Ok(TurnOutcome::Exhausted) => {
                em.agent_done(child.id, child_agent_id, parent_agent_id, "⚠️ Exhausted tool-call rounds.".to_string()).await;
                Ok(format!(
                    "Sub-agent `{target_id}` exceeded {} tool-call rounds without producing a final answer.",
                    self.max_tool_rounds
                ))
            }
            Err(e) => {
                let msg = e.to_string();
                em.agent_done(child.id, child_agent_id, parent_agent_id, format!("⚠️ Error: {msg}")).await;
                Err(e)
            }
        };

        let _ = chat_sessions_stack::terminate(pool, child.id).await;
        result
    }

    /// Builds the [`AgentRunConfig`] for a sub-agent stack frame: base tools derived
    /// from `parent_config`, plus the sub-agent augmentation (sub-agents-only tools,
    /// `ask_user_clarification`, `execute_subtask` while `depth` still permits
    /// recursion), the approval-visibility filter, the frame's persisted MCP grants,
    /// and a stack-scoped `activate_tools`.
    ///
    /// The **single** source of a sub-agent's config, shared by live dispatch
    /// (`dispatch_sub_agent`) and post-restart recovery (`build_recovery_frame_config`),
    /// so a resumed child runs with the same prompt/tools it had live — never the root
    /// agent's (bug B3). `depth` is passed explicitly (not `parent.depth + 1`) so
    /// recovery can build a config for a frame at any depth straight from the root.
    pub(super) async fn build_sub_agent_config(
        &self,
        parent_config: &AgentRunConfig,
        agent_id:      &str,
        client_name:   String,
        stack_id:      i64,
        depth:         i64,
    ) -> anyhow::Result<AgentRunConfig> {
        let persisted_grants = activated_tools::list_refs_stack(&self.db, stack_id)
            .await
            .unwrap_or_default();
        let active_mcp_grants: Arc<RwLock<HashSet<String>>> =
            Arc::new(RwLock::new(persisted_grants.into_iter().collect()));

        let mut child_config = parent_config.for_sub_agent(agent_id.to_string(), client_name);
        child_config.depth             = depth;
        child_config.active_mcp_grants = Arc::clone(&active_mcp_grants);

        child_config.base_tool_defs.extend(self.tools.openai_definitions_sub_agents_only());
        child_config.base_tool_defs.push(super::ask_user_clarification_tool_def());
        // Expose `execute_subtask` only while the child can still recurse — at the
        // depth limit `dispatch_sub_agent` would reject it.
        if depth < MAX_AGENT_DEPTH {
            child_config.base_tool_defs.push(super::execute_subtask_tool_def());
        }

        {
            let group_id    = self.tool_group_id().await;
            let gid         = group_id.as_deref().unwrap_or("default");
            // Registry table — read from the registry pool, not the owner pool
            // (see the same filter in `config.rs::build_agent_config`).
            let group_rules = match crate::db::approval_rules::list_for_group(
                &self.shared_pool, Some(gid),
            ).await {
                Ok(rules) => rules,
                Err(e) => {
                    tracing::warn!(group = gid, error = %e, "sub-agent approval-rules visibility filter: list_for_group failed; leaving all tools visible");
                    Vec::new()
                }
            };
            child_config.base_tool_defs.retain(|def| {
                let name = def["function"]["name"].as_str().unwrap_or("");
                self.approval.is_tool_visible(&group_rules, name)
            });
        }

        {
            let activate_tool = crate::tools::activate_tools::ActivateTools {
                stack_id:          Some(stack_id),
                mcp:               Arc::clone(&self.mcp),
                active_mcp_grants: Arc::clone(&active_mcp_grants),
            };
            let activate_tool = Arc::new(activate_tool);
            child_config.interface_tools.push(InterfaceTool {
                definition: activate_tools_tool_def(),
                handler: Arc::new(move |args| -> ToolFuture {
                    use crate::tools::Tool as _;
                    let tool = Arc::clone(&activate_tool);
                    Box::pin(async move {
                        tokio::task::spawn_blocking(move || tool.execute(args))
                            .await
                            .map_err(|e| anyhow::anyhow!("activate_tools task panicked: {e}"))?
                    })
                }),
            });
        }

        Ok(child_config)
    }

    /// Config to re-run a sub-agent frame during app-restart recovery: resolves the
    /// frame's **own** agent (prompt/meta/client) and builds its sub-agent config, so
    /// `resume_turn`'s cascade resumes a child as itself, not as the root agent (bug
    /// B3). The root frame is not passed here — the caller keeps the session's root
    /// config for it. Base tools derive from `root_config`; the per-dispatch `client`
    /// override isn't persisted, so the frame's agent meta drives model resolution.
    pub(super) async fn build_recovery_frame_config(
        &self,
        root_config: &AgentRunConfig,
        frame:       &chat_sessions_stack::SessionStack,
    ) -> anyhow::Result<AgentRunConfig> {
        let meta = crate::agents::load_task_meta(&frame.agent_id)
            .map_err(|e| anyhow::anyhow!("resume: cannot load sub-agent `{}`: {e}", frame.agent_id))?;
        let (client, _) = self.llm_manager.resolve(
            meta.client.as_deref(), meta.strength,
        ).await?;
        self.build_sub_agent_config(root_config, &frame.agent_id, client.to_string(), frame.id, frame.depth).await
    }

    /// Handles the `update_scratchpad` built-in.
    ///
    /// The scratchpad is a session-scoped shared blackboard (`scratchpad_sid()` is
    /// the session_id, identical for every frame). When a homogeneous batch of
    /// sub-agents runs concurrently (`handle_sub_agent_batch`), two siblings writing
    /// the *same* key race to last-writer-wins — this is inherent to a shared
    /// blackboard and accepted by design, not a correctness bug. Sub-agents that must
    /// not clobber each other should write distinct keys.
    pub(super) async fn dispatch_update_scratchpad(
        &self,
        args: &Value,
    ) -> anyhow::Result<String> {
        let key   = args["key"].as_str().unwrap_or("").to_string();
        let value = args["value"].as_str().unwrap_or("").to_string();
        scratchpad::upsert(&self.db, self.scratchpad_sid(), &key, &value).await
            .map(|_| format!("Scratchpad updated: {key}"))
    }

    /// Handles the `write_todos` built-in.
    ///
    /// Stateless: the list is not persisted anywhere — it lives only in this
    /// agent's tool-result history (per-stack, so it is never seen by sub-agents
    /// or the caller). We just validate/normalise the items and echo back a
    /// formatted checklist the model re-reads from its own tool result.
    pub(super) async fn dispatch_write_todos(
        &self,
        args: &Value,
    ) -> anyhow::Result<String> {
        let items = args["todos"].as_array().ok_or_else(|| {
            anyhow::anyhow!("`write_todos` requires a `todos` array. Re-send the full list, e.g. [{{\"content\":\"...\",\"status\":\"pending\"}}].")
        })?;
        if items.is_empty() {
            return Err(anyhow::anyhow!("`todos` is empty — send at least one item, or omit the call entirely."));
        }

        let mut lines = Vec::with_capacity(items.len());
        let (mut done, mut active, mut pending) = (0usize, 0usize, 0usize);
        for item in items {
            let content = item["content"].as_str().unwrap_or("").trim();
            if content.is_empty() {
                continue;
            }
            // Normalise unknown statuses to `pending`.
            let marker = match item["status"].as_str() {
                Some("completed")   => { done   += 1; "x" }
                Some("in_progress") => { active += 1; "~" }
                _                   => { pending += 1; " " }
            };
            lines.push(format!("[{marker}] {content}"));
        }
        if lines.is_empty() {
            return Err(anyhow::anyhow!("No valid todo items (every `content` was empty)."));
        }

        Ok(format!(
            "Todo list ({total}): {done} done, {active} in progress, {pending} pending\n{body}",
            total = lines.len(),
            body  = lines.join("\n"),
        ))
    }

    /// Handles the `ask_user_clarification` built-in.
    ///
    /// Interactive sessions (web, telegram): sends `AgentQuestion` over the WS channel
    /// and waits for the user to answer inline in the chat.
    ///
    /// Background sessions (cron, tic): registers in `ClarificationManager` so the
    /// Agent Inbox page can surface and resolve the request.
    ///
    /// `tool_call_id` is used to mark the DB row as `pending` before blocking,
    /// so page refreshes and app restarts can distinguish "waiting for input" from
    /// "was executing" and re-ask the question correctly.
    pub(super) async fn dispatch_ask_user_clarification(
        &self,
        tool_call_id: i64,
        args: &Value,
        tx:   &mpsc::Sender<ServerEvent>,
    ) -> anyhow::Result<String> {
        let title    = args["title"].as_str().unwrap_or("Clarification needed").to_string();
        let question = args["question"].as_str().unwrap_or("?").to_string();
        let suggested: Vec<String> = args["suggested_answers"]
            .as_array()
            .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
            .unwrap_or_default();

        // Mark as pending before suspending so restart/refresh can re-ask the question.
        chat_llm_tools::set_approval_pending(&self.db, tool_call_id).await?;

        let context_label = self.context_label.read().ok().and_then(|g| g.clone());

        // Always register in ClarificationManager so the question appears in the
        // Agent Inbox for ALL sessions (both interactive web/telegram and background cron/tic).
        let (request_id, rx) = self.clarification.register(
            self.session_id,
            &self.agent_id,
            &self.source,
            context_label.as_deref(),
            &title,
            &question,
            suggested.clone(),
        ).await;

        tracing::debug!(session_id = self.session_id, request_id, is_interactive = self.is_interactive, source = %self.source, "dispatch_ask_user_clarification: routing");
        if self.is_interactive {
            // For interactive sessions, also send the question over WS so it appears
            // inline in the chat. The user can answer from either the chat or the Inbox.
            info!(session_id = self.session_id, request_id, %question, source = %self.source, "agent asking user for clarification (interactive) — sending AgentQuestion");
            let send_result = tx.send(ServerEvent::AgentQuestion {
                request_id,
                tool_call_id,
                title,
                question,
                suggested_answers: suggested,
            }).await;
            if send_result.is_err() {
                tracing::warn!(session_id = self.session_id, request_id, "AgentQuestion send failed — tx receiver dropped");
            } else {
                info!(session_id = self.session_id, request_id, "AgentQuestion sent to bridge");
            }
        } else {
            info!(session_id = self.session_id, request_id, %question, source = %self.source, "background session waiting for clarification");
        }

        // Wait for the answer (from WS via resolve_question → clarification.resolve,
        // or directly from the Inbox REST endpoint).
        rx.await.map_err(|_| anyhow::Error::new(super::AgentFlowSignal::QuestionChannelClosed))
    }
}
