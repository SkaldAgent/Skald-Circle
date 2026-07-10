use crate::provider::ServiceType;

/// Light umbrella trait shared by all model managers.
/// Enables grouping managers generically (e.g. models-hub routing, diagnostics)
/// without coupling to their specific CRUD operations.
pub trait ServiceManager: Send + Sync {
    fn service_type(&self) -> ServiceType;
    fn display_name(&self) -> &'static str;
}
