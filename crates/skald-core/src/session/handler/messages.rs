use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use serde_json::Value;

use crate::llm::DtlMode;
use super::ChatSessionHandler;
use super::message_builder::MessageBuilder;

impl ChatSessionHandler {
    /// Thin wrapper: constructs a `MessageBuilder` from this handler's fields
    /// and delegates to `MessageBuilder::build`.
    ///
    /// See `MessageBuilder::build` for the full documentation and message ordering.
    pub(super) async fn build_openai_messages(
        &self,
        pool:                 &sqlx::SqlitePool,
        stack_id:             i64,
        agent_id:             &str,
        extra_system_static:  Option<&str>,
        extra_system_dynamic: Option<&str>,
        tail_reminder:        Option<&str>,
        active_mcp_grants:    &HashSet<String>,
        system_substitutions: &HashMap<String, String>,
        cache_hints:          bool,
        capabilities:         &[String],
        dtl:                  DtlMode,
        config_tool_defs:     &[Value],
        activation_stack:     Option<i64>,
    ) -> anyhow::Result<Vec<Value>> {
        let project_root = self.run_context.read().await
            .as_ref()
            .and_then(|rc| rc.project_root.clone());
        let builder = MessageBuilder {
            pool:                  Arc::clone(&self.db),
            shared_pool:           Arc::clone(&self.shared_pool),
            user_id:               self.user_id.clone(),
            session_id:            self.scratchpad_sid(),
            mcp:                   Arc::clone(&self.mcp),
            datetime_config:       self.datetime_config.clone(),
            max_history_messages:  self.max_history_messages,
            max_tool_result_chars: self.max_tool_result_chars,
            compactor:             self.compactor.clone(),
            project_root,
            // Snapshot the fs cell for this build — its workspace roots contain the
            // tool-produced media inlined into the current turn (§6 remount-safe).
            fs:                    Some(self.fs.load()),
        };
        // `pool` is passed in from the caller (always `&self.db`) but we take
        // ownership via Arc::clone above so the signature stays backward-compatible.
        let _ = pool; // suppress unused-variable warning; MessageBuilder uses its own Arc
        builder.build(stack_id, agent_id, extra_system_static, extra_system_dynamic, tail_reminder, active_mcp_grants, system_substitutions, cache_hints, capabilities, dtl, config_tool_defs, activation_stack).await
    }
}
