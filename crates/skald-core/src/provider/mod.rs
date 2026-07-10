// All provider types and traits now live in core-api.
// Re-export everything so existing imports in this crate continue to work.
pub use core_api::provider::{
    ApiProvider, ApiProviderRegistry, BuiltLlmClient,
    LlmModelRecord, LlmProviderRecord, LlmStrength,
    ProviderField, ProviderUiMeta, ReasoningMode, ServiceType,
    RemoteLlmModelInfo,
};

// ── ProviderRegistry ──────────────────────────────────────────────────────────

use std::sync::Arc;
use core_api::system_bus::{SystemEvent, SystemEventBus};

pub struct ProviderRegistry {
    builtin:    Vec<Arc<dyn ApiProvider>>,
    plugins:    std::sync::RwLock<Vec<Arc<dyn ApiProvider>>>,
    system_bus: Arc<SystemEventBus>,
}

impl ProviderRegistry {
    pub fn new(system_bus: Arc<SystemEventBus>) -> Self {
        Self {
            builtin: Vec::new(),
            plugins: std::sync::RwLock::new(Vec::new()),
            system_bus,
        }
    }

    pub fn register_builtin(&mut self, p: impl ApiProvider + 'static) {
        self.builtin.push(Arc::new(p));
    }

    /// Looks up a provider by type_id. Plugin providers shadow built-in ones.
    pub fn get(&self, type_id: &str) -> Option<Arc<dyn ApiProvider>> {
        {
            let plugins = self.plugins.read().unwrap();
            if let Some(p) = plugins.iter().find(|p| p.type_id() == type_id) {
                return Some(Arc::clone(p));
            }
        }
        self.builtin.iter().find(|p| p.type_id() == type_id).cloned()
    }

    /// Returns all known providers: plugin-registered first, then built-in.
    pub fn all(&self) -> Vec<Arc<dyn ApiProvider>> {
        let plugins = self.plugins.read().unwrap();
        let mut result: Vec<Arc<dyn ApiProvider>> = plugins.clone();
        for p in &self.builtin {
            if !result.iter().any(|x| x.type_id() == p.type_id()) {
                result.push(Arc::clone(p));
            }
        }
        result
    }

    pub fn contains(&self, type_id: &str) -> bool {
        self.get(type_id).is_some()
    }
}

impl core_api::provider::ApiProviderRegistry for ProviderRegistry {
    fn register_plugin(&self, p: Arc<dyn ApiProvider>) {
        let id = p.type_id();
        let mut plugins = self.plugins.write().unwrap();
        plugins.retain(|x| x.type_id() != id);
        plugins.push(p);
        tracing::info!(type_id = id, "provider registered (plugin)");
        self.system_bus.send(SystemEvent::ApiProviderRegistered { type_id: id.to_string() });
    }

    fn unregister_plugin(&self, type_id: &str) {
        self.plugins.write().unwrap().retain(|p| p.type_id() != type_id);
        tracing::info!(type_id, "provider unregistered (plugin)");
        self.system_bus.send(SystemEvent::ApiProviderUnregistered { type_id: type_id.to_string() });
    }
}
