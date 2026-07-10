use std::sync::Arc;

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use core_api::PropertyType;
use core_api::system_bus::SystemEvent;

use crate::core::skald::Skald;
use super::ApiError;

// ── Response types ─────────────────────────────────────────────────────────────

#[derive(Serialize, Clone)]
struct SecurityGroupOption {
    id:   String,
    name: String,
}

#[derive(Serialize)]
struct PropertyView {
    key:           String,
    name:          String,
    description:   String,
    property_type: String,
    value:         Option<String>,
    default_value: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    options:       Option<Vec<SecurityGroupOption>>,
}

#[derive(Serialize)]
struct ConfigSetView {
    name:        String,
    description: String,
    properties:  Vec<PropertyView>,
}

// ── GET /api/config ────────────────────────────────────────────────────────────

pub async fn list_properties(
    State(skald): State<Arc<Skald>>,
) -> Result<Json<Value>, ApiError> {
    let security_groups = skald.run_context_manager().list_groups().await
        .unwrap_or_default()
        .into_iter()
        .map(|g| SecurityGroupOption { id: g.id, name: g.name })
        .collect::<Vec<_>>();

    let mut sets = Vec::with_capacity(skald.config_properties().len());
    for set in skald.config_properties() {
        let mut props = Vec::with_capacity(set.properties.len());
        for prop in &set.properties {
            let value = skald.config().get(&prop.key).await?;
            let (type_str, options) = match prop.property_type {
                PropertyType::Int           => ("int", None),
                PropertyType::Bool          => ("bool", None),
                PropertyType::String        => ("string", None),
                PropertyType::SecurityGroup => ("security_group", Some(security_groups.clone())),
            };
            props.push(PropertyView {
                key:           prop.key.clone(),
                name:          prop.name.clone(),
                description:   prop.description.clone(),
                property_type: type_str.into(),
                value,
                default_value: prop.default_value.clone(),
                options,
            });
        }
        sets.push(ConfigSetView {
            name:        set.name.clone(),
            description: set.description.clone(),
            properties:  props,
        });
    }

    Ok(Json(json!({ "sets": sets })))
}

// ── PUT /api/config/:key ────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct SetPropertyBody {
    pub value: String,
}

#[derive(Deserialize)]
pub struct KeyPath {
    pub key: String,
}

pub async fn set_property(
    State(skald): State<Arc<Skald>>,
    Path(p): Path<KeyPath>,
    Json(body): Json<SetPropertyBody>,
) -> Result<StatusCode, ApiError> {
    // Only allow keys that are registered as config properties.
    let known = skald.config_properties().iter()
        .flat_map(|s| &s.properties)
        .any(|prop| prop.key == p.key);
    if !known {
        return Err(ApiError::not_found("unknown config key"));
    }

    let old_value = skald.config().get(&p.key).await?;

    // No-op if value didn't change.
    if old_value.as_deref() == Some(body.value.as_str()) {
        return Ok(StatusCode::OK);
    }

    skald.config().set(&p.key, &body.value).await?;

    skald.system_bus().send(SystemEvent::ConfigKeyUpdated {
        key:       p.key.clone(),
        old_value,
        new_value: body.value,
    });

    Ok(StatusCode::OK)
}
