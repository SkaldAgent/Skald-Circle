//! Skald-side adapters implementing the `agent-loop` trait surface: everything
//! the library asks a host for, answered the way Skald does it. The loop itself
//! — rounds, projection, delegation, recovery, compaction — is the crate's.
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
//! - [`projection_cfg`] — the wire knobs Skald's models need, handed to the
//!   library's projection engine, plus the assembler every turn runs on.
//!   Skald owns no projection code: [`media_source`] authorizes which files may
//!   be inlined (§6 containment) and [`tool_digest`] condenses an over-long
//!   tool result — the library does the shaping.
//! - [`async_task`] — `execute_task mode=async` as a durable cron job, and the
//!   delivery of its result back into the parent conversation (§7.2).
//! - [`prefix_cache`] — the cacheable half of the system prompt, frozen per
//!   conversation so a mid-turn memory write does not invalidate the provider's
//!   prompt cache.
//! - [`runtime::UserLoopRuntime`] — the one `LoopManager` per user (D12) these
//!   are all assembled into, plus the per-turn parameters.

pub mod activation;
pub mod async_task;
pub mod builtins;
pub mod catalog;
pub mod gate;
pub mod history;
pub mod hooks;
pub mod live_input;
pub mod media_source;
pub mod prefix_cache;
pub mod preview;
#[cfg(test)]
mod projection_snapshots;
pub mod scope;
pub mod projection_cfg;
pub mod runtime;
pub mod selector;
pub mod system;
#[cfg(test)]
mod testkit;
pub mod tool_digest;
pub mod toolset;
pub mod translate;
