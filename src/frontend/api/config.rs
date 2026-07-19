use std::sync::Arc;

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use core_api::PropertyType;

use skald_core::skald::Skald;
use super::ApiError;

// ── Response types ─────────────────────────────────────────────────────────────

/// One choice in a dropdown-style property (see [`PropertyType`]). Deliberately
/// generic — `id` is the stored value, `name` the human label — so every custom
/// "pick from a fixed/derived set" property type reuses it (security groups,
/// locales, and whatever the next section needs).
#[derive(Serialize, Clone)]
struct SelectOption {
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
    options:       Option<Vec<SelectOption>>,
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
    // Option sources for the dropdown-style property types. Each custom
    // `PropertyType` that renders as a `<select>` computes its choices here and
    // ships them in `options`. To add a new one: build its `Vec<SelectOption>`
    // and wire it into the `match` below (see `PropertyType` for the full
    // three-step recipe).
    let security_groups = skald.run_context_manager().list_groups().await
        .unwrap_or_default()
        .into_iter()
        .map(|g| SelectOption { id: g.id, name: g.name })
        .collect::<Vec<_>>();
    let locales = skald_core::i18n::SUPPORTED_LOCALES.iter()
        .map(|code| SelectOption {
            id:   code.to_string(),
            name: skald_core::i18n::native_language_name(code),
        })
        .collect::<Vec<_>>();

    let mut sets = Vec::with_capacity(skald.config_properties().len());
    for set in skald.config_properties() {
        let mut props = Vec::with_capacity(set.properties.len());
        for prop in &set.properties {
            let value = skald.config().get(&prop.key).await?;
            // Scalars carry no `options`; dropdown types attach their choices.
            let (type_str, options) = match prop.property_type {
                PropertyType::Int           => ("int", None),
                PropertyType::Bool          => ("bool", None),
                PropertyType::String        => ("string", None),
                PropertyType::SecurityGroup => ("security_group", Some(security_groups.clone())),
                PropertyType::Locale        => ("locale", Some(locales.clone())),
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

    // GlobalConfigManager::set handles the no-op check and emits
    // ConfigKeyUpdated on the system bus when the value actually changes.
    skald.config().set(&p.key, &body.value).await?;

    Ok(StatusCode::OK)
}
