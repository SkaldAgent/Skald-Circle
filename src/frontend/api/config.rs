//! The instance's settings.
//!
//! **Admin-only, and enforced here.** Every key on this surface is instance-wide
//! — the interface language, which model summarises history, how often a
//! background agent runs for everybody — so there is no reading of it that makes
//! sense for a member. The sidebar has always hidden the page from non-admins,
//! which is presentation, not authorization: until these handlers took the
//! caller into account at all, any authenticated session could read *and write*
//! them.
//!
//! A set carrying a [`ConfigSet::owner`] is **not** served here: it belongs to
//! the page that owns it (see [`render_sets`], reused by that page so the two
//! render identically).

use std::sync::Arc;

use axum::{
    Extension, Json,
    extract::{Path, State},
    http::StatusCode,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use core_api::{ConfigSet, PropertyType};

use skald_core::skald::Skald;
use super::guard::AuthUser;
use super::{ApiError, caps};

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
pub struct ConfigSetView {
    name:        String,
    description: String,
    properties:  Vec<PropertyView>,
}

// ── GET /api/config ────────────────────────────────────────────────────────────

pub async fn list_properties(
    State(skald):    State<Arc<Skald>>,
    Extension(auth): Extension<AuthUser>,
) -> Result<Json<Value>, ApiError> {
    caps::require_admin(&skald, &auth.user_id).await?;

    // Owned sets are edited on the surface that owns them, not here.
    let sets: Vec<&ConfigSet> = skald
        .config_properties()
        .iter()
        .filter(|s| s.owner.is_none())
        .collect();

    Ok(Json(json!({ "sets": render_sets(&skald, &sets).await? })))
}

/// Resolve every property in `sets` to its current value plus, for the dropdown
/// types, the choices the backend owns.
///
/// Shared with the System agents page so an owned set renders exactly like one
/// on the Config page — same types, same options, same defaults. The caller is
/// responsible for authorization: this function assumes it has already happened.
pub async fn render_sets(
    skald: &Skald,
    sets:  &[&ConfigSet],
) -> Result<Vec<ConfigSetView>, ApiError> {
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
    // Configured LLM models, keyed by `name` (the resolution key LlmManager
    // uses), labelled with the provider for disambiguation.
    let llm_models = skald.llm_manager().list_models_info().await
        .into_iter()
        .map(|m| SelectOption {
            id:   m.name.clone(),
            name: format!("{} ({})", m.name, m.provider_name),
        })
        .collect::<Vec<_>>();

    let mut views = Vec::with_capacity(sets.len());
    for set in sets {
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
                PropertyType::LlmModel      => ("llm_model", Some(llm_models.clone())),
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
        views.push(ConfigSetView {
            name:        set.name.clone(),
            description: set.description.clone(),
            properties:  props,
        });
    }

    Ok(views)
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

/// `PUT /api/config/{key}` — write one instance-wide setting.
///
/// The single write path for **every** config property, owned sets included: the
/// System agents page edits its tabs through this endpoint rather than one of
/// its own, so the admin gate and the known-key check exist in one place.
pub async fn set_property(
    State(skald):    State<Arc<Skald>>,
    Extension(auth): Extension<AuthUser>,
    Path(p): Path<KeyPath>,
    Json(body): Json<SetPropertyBody>,
) -> Result<StatusCode, ApiError> {
    caps::require_admin(&skald, &auth.user_id).await?;

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
