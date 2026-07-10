use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use serde_json::Value;

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
    ) -> anyhow::Result<Vec<Value>> {
        let effective_wd = self.run_context.read().await
            .as_ref()
            .map(|rc| rc.effective_working_dir());
        let builder = MessageBuilder {
            pool:                  Arc::clone(&self.db),
            session_id:            self.scratchpad_sid(),
            mcp:                   Arc::clone(&self.mcp),
            datetime_config:       self.datetime_config.clone(),
            max_history_messages:  self.max_history_messages,
            max_tool_result_chars: self.max_tool_result_chars,
            compactor:             self.compactor.clone(),
            working_directory:     effective_wd,
        };
        // `pool` is passed in from the caller (always `&self.db`) but we take
        // ownership via Arc::clone above so the signature stays backward-compatible.
        let _ = pool; // suppress unused-variable warning; MessageBuilder uses its own Arc
        builder.build(stack_id, agent_id, extra_system_static, extra_system_dynamic, tail_reminder, active_mcp_grants, system_substitutions, cache_hints).await
    }
}
