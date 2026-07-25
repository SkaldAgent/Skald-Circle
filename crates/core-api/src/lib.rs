/// Application name, sent as `X-Title` HTTP header to LLM/image/audio providers.
/// Lives in `agent-loop` (the LLM clients' home, blueprint D13); re-exported here
/// so existing users don't change.
pub use agent_loop::APP_NAME;

pub mod approval;
pub mod bus;
pub mod config_api;
pub mod system_bus;
pub mod chat_hub;
pub mod command;
pub mod events;
pub mod i18n;
pub mod image_generate;
pub mod inbox;
pub mod interface_tool;
pub mod location;
pub mod memory;
pub mod message_meta;
pub mod plugin;
pub mod provider;
pub mod remote;
pub mod tool;
pub mod user_channel;
pub mod user_fs;
pub mod user_plugin_config;
pub mod secrets;
pub mod transcribe;
pub mod tts;
pub mod config_property;
pub use config_property::{ConfigProperty, ConfigSet, PropertyType};
