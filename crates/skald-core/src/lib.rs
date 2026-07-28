//! The headless Skald core: storage, identity, LLM stack, tools, sessions.
//!
//! Nothing here knows what runs it. The process shell — HTTP server, setup
//! wizard — lives in the crates that depend on this one. Concrete
//! plugins are never named: `plugin::PluginManager` only ever sees
//! `Arc<dyn Plugin>`, constructed by the consumer and handed to `Skald::new`.

pub mod boot;
pub mod config;
pub mod auth;
pub mod config_store;
pub mod skald;
pub mod agents;
pub mod approval;
pub mod chat_event_bus;
pub mod chat_hub;
pub mod clarification;
pub mod command;
pub mod compactor;
pub mod container;
pub mod crypto;
pub mod elicitation;
pub mod cron;
pub mod db;
pub mod events;
pub mod image_generate;
pub mod i18n;
pub mod inbox;
pub mod latex;
pub mod llm;
pub mod location;
pub mod loop_adapters;
pub mod memory;
pub mod mcp;
pub mod notification;
pub mod pending_registry;
pub mod plugin;
pub mod projects;
pub mod provider;
pub mod run_context;
pub mod secrets;
pub mod service_manager;
pub mod session;
pub mod setup;
pub mod system_agents;
pub mod event_triage;
pub mod tool_catalog;
pub mod tool_discovery;
pub mod tools;
pub mod transcribe;
pub mod tts;
pub mod uploads;
pub mod users;
