/// Application name, sent as `X-Title` HTTP header to LLM/image/audio providers.
pub const APP_NAME: &str = "Skald";

pub mod approval;
pub mod bus;
pub mod config_api;
pub mod system_bus;
pub mod chatbot;
pub mod chat_hub;
pub mod command;
pub mod events;
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
pub mod secrets;
pub mod transcribe;
pub mod tts;
pub mod config_property;
pub use config_property::{ConfigProperty, ConfigSet, PropertyType};
