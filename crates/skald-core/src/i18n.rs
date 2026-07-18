//! UI localization knobs.
//!
//! The instance default locale lives in the registry `config` table under
//! [`DEFAULT_LOCALE_KEY`], editable by the admin from the Settings page. Each
//! user can override it on their own profile (`users.locale`); the frontend
//! resolves user → instance → built-in English at boot.

use core_api::{ConfigProperty, ConfigSet, PropertyType};

pub const DEFAULT_LOCALE_KEY: &str = "ui_locale";

/// Locales the web UI ships dictionaries for. Anything else is rejected at
/// write time (profile override, first-run setup) rather than silently
/// falling back to English later.
pub const SUPPORTED_LOCALES: &[&str] = &["en", "it", "fr"];

pub fn is_supported(locale: &str) -> bool {
    SUPPORTED_LOCALES.contains(&locale)
}

/// Writes the instance default locale straight to the registry `config` table.
/// Used by first-run provisioning shells (e.g. `skald-setup`), where no
/// `GlobalConfigManager` — hence no system bus — exists. A running server
/// should go through `GlobalConfigManager::set` instead, which also emits the
/// change event.
pub async fn set_default_locale(pool: &sqlx::SqlitePool, locale: &str) -> anyhow::Result<()> {
    anyhow::ensure!(is_supported(locale), "unsupported locale: {locale}");
    sqlx::query(
        "INSERT INTO config (key, value, updated_at) VALUES (?, ?, datetime('now'))
         ON CONFLICT(key) DO UPDATE SET
             value      = excluded.value,
             updated_at = excluded.updated_at",
    )
    .bind(DEFAULT_LOCALE_KEY)
    .bind(locale)
    .execute(pool)
    .await?;
    Ok(())
}

pub fn config_set() -> ConfigSet {
    ConfigSet {
        name:        "Interface".into(),
        description: "Look and feel of the web interface.".into(),
        properties:  vec![
            ConfigProperty {
                key:           DEFAULT_LOCALE_KEY.into(),
                name:          "Language".into(),
                description:   "Default interface language for the whole instance (e.g. en, it). Each user can override it on their profile.".into(),
                property_type: PropertyType::String,
                default_value: Some("en".into()),
            },
        ],
    }
}
