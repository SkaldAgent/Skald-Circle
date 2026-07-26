//! Skald-side adapters implementing the `agent-loop` trait surface over the
//! existing infrastructure (blueprint §14 phase 1). **Unused by the current
//! loop** — they compile and are unit-tested here, and get wired in phase 2.
//!
//! - [`history::SqliteHistory`] — `HistoryStore` over the existing
//!   `chat_sessions_stack` / `chat_history` / `chat_llm_tools` / `chat_summaries`
//!   tables (no migration, §0).
//! - [`selector::SkaldSelector`] — `ModelSelector` over `LlmManager`, with the
//!   agent's strength captured per-turn (D14).
//! - [`gate::ApprovalGate`] — `Gate` over `ApprovalManager` + the RunContext
//!   fast-path + auto-deny + pre-approved (port of `handler/gate.rs`).
//! - [`toolset::SkaldToolSet`] — `ToolSet` over base/config defs + MCP grants +
//!   memory/image/interface tools, with DTL rendering (port of
//!   `AgentRunConfig::all_tool_defs`), plus the core-api→agent-loop tool bridge.
//! - [`activation`] — `ActivationSource` + `ToolActivator` over the
//!   `activated_tools` table and the MCP provider (D15).

pub mod activation;
pub mod gate;
pub mod history;
pub mod selector;
pub mod toolset;
