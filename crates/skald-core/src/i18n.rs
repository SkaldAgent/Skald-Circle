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

/// The instance default locale (registry `config.ui_locale`), `"en"` when
/// unset. A read — no system bus involved — so it works from any context that
/// has a pool (sessions, shells), not just where a `GlobalConfigManager`
/// lives. An unreadable config table degrades to `"en"` rather than failing
/// the caller.
pub async fn default_locale(pool: &sqlx::SqlitePool) -> String {
    crate::db::config::get(pool, DEFAULT_LOCALE_KEY)
        .await
        .ok()
        .flatten()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "en".into())
}

/// The effective locale for a user: their `users.locale` override when set,
/// the instance default otherwise (which itself falls back to `"en"`).
/// **The** resolution chain — call this instead of re-implementing it.
pub async fn resolve_locale(pool: &sqlx::SqlitePool, user_locale: Option<&str>) -> String {
    match user_locale.map(str::trim).filter(|s| !s.is_empty()) {
        Some(l) => l.to_string(),
        None    => default_locale(pool).await,
    }
}

/// Human language name for prompt rendering (`"it"` → `"Italian"`). Unknown
/// codes pass through unchanged — the model copes, and this list need not
/// track every locale ever stored.
pub fn language_name(locale: &str) -> String {
    match locale {
        "en" => "English".into(),
        "it" => "Italian".into(),
        "fr" => "French".into(),
        other => other.into(),
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_db_path(tag: &str) -> String {
        let mut p = std::env::temp_dir();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        p.push(format!("skald-test-i18n-{tag}-{}-{nanos}", std::process::id()));
        p.push("database");
        p.push("system.db");
        p.to_string_lossy().into_owned()
    }

    fn cleanup(path: &str) {
        if let Some(dir) = std::path::Path::new(path).parent().and_then(|p| p.parent()) {
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    #[test]
    fn language_name_maps_known_codes_and_passes_through_unknown() {
        assert_eq!(language_name("en"), "English");
        assert_eq!(language_name("it"), "Italian");
        assert_eq!(language_name("fr"), "French");
        assert_eq!(language_name("de"), "de");
    }

    #[tokio::test]
    async fn resolve_locale_follows_user_then_instance_then_builtin() {
        let path = temp_db_path("resolve");
        let pool = crate::db::init_system_pool(&path).await.unwrap();

        // No override, no instance default → built-in English.
        assert_eq!(resolve_locale(&pool, None).await, "en");
        assert_eq!(default_locale(&pool).await, "en");

        // Instance default kicks in when the user has no override.
        set_default_locale(&pool, "it").await.unwrap();
        assert_eq!(resolve_locale(&pool, None).await, "it");
        assert_eq!(resolve_locale(&pool, Some("  ")).await, "it", "blank override counts as none");

        // The user override always wins.
        assert_eq!(resolve_locale(&pool, Some("fr")).await, "fr");

        pool.close().await;
        cleanup(&path);
    }
}
