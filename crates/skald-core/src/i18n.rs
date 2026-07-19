//! UI localization knobs.
//!
//! The instance default locale lives in the registry `config` table under
//! [`DEFAULT_LOCALE_KEY`], editable by the admin from the Settings page. Each
//! user can override it on their own profile (`users.locale`); the frontend
//! resolves user → instance → built-in English at boot.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use core_api::i18n::{I18nApi, LocaleBundle};
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

/// Native (endonym) language name for UI language pickers (`"it"` → `"Italiano"`).
/// Unlike [`language_name`] — an English exonym for prompt rendering — this is
/// what a user expects to see when *choosing* their language, and it reads the
/// same regardless of the interface language currently active. Unknown codes
/// pass through unchanged.
pub fn native_language_name(locale: &str) -> String {
    match locale {
        "en" => "English".into(),
        "it" => "Italiano".into(),
        "fr" => "Français".into(),
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

/// The backend translation catalog — the concrete [`I18nApi`] injected into
/// every `PluginContext`. Built once at boot by merging every plugin's
/// [`core_api::plugin::Plugin::i18n`] bundles, keyed by locale. Lookups follow
/// the same chain as the frontend `t()`: resolved locale → English → the raw
/// key, with `{name}` placeholders filled from `args`.
///
/// Immutable after construction: bundles are collected before any request, so
/// no lock is needed on the read path (`get` is a plain map lookup).
pub struct I18nCatalog {
    /// System pool — reads `users.locale` and the instance-default `config` key
    /// to resolve a user's effective locale (see [`resolve_locale`]).
    pool:   Arc<sqlx::SqlitePool>,
    /// locale → (key → string).
    tables: HashMap<String, HashMap<String, String>>,
}

impl I18nCatalog {
    /// Merge `bundles` into one catalog. Two bundles for the same locale union
    /// their keys (later wins on a collision — the `plugin.<id>.` convention
    /// keeps collisions to genuine overrides).
    pub fn new(pool: Arc<sqlx::SqlitePool>, bundles: Vec<LocaleBundle>) -> Self {
        let mut tables: HashMap<String, HashMap<String, String>> = HashMap::new();
        for b in bundles {
            tables.entry(b.locale).or_default().extend(b.strings);
        }
        Self { pool, tables }
    }

    fn lookup(&self, locale: &str, key: &str) -> Option<&str> {
        self.tables.get(locale).and_then(|m| m.get(key)).map(String::as_str)
    }

    /// Resolve → fall back to English → fall back to the key itself, then fill
    /// `{name}` placeholders.
    fn render(&self, locale: &str, key: &str, args: &[(&str, &str)]) -> String {
        let raw = self
            .lookup(locale, key)
            .or_else(|| self.lookup("en", key))
            .unwrap_or(key);
        let mut s = raw.to_string();
        for (k, v) in args {
            s = s.replace(&format!("{{{k}}}"), v);
        }
        s
    }
}

#[async_trait]
impl I18nApi for I18nCatalog {
    async fn for_user(&self, user_id: &str, key: &str, args: &[(&str, &str)]) -> String {
        let user_locale = crate::db::users::get(&self.pool, user_id)
            .await
            .ok()
            .flatten()
            .and_then(|u| u.locale);
        let locale = resolve_locale(&self.pool, user_locale.as_deref()).await;
        self.render(&locale, key, args)
    }

    fn get(&self, locale: &str, key: &str, args: &[(&str, &str)]) -> String {
        self.render(locale, key, args)
    }
}

pub fn config_set() -> ConfigSet {
    ConfigSet {
        name:        "Interface".into(),
        description: "Look and feel of the web interface.".into(),
        properties:  vec![
            ConfigProperty {
                key:           DEFAULT_LOCALE_KEY.into(),
                name:          "Language".into(),
                description:   "Default interface language for the whole instance. Each user can override it on their profile.".into(),
                // A dropdown of `SUPPORTED_LOCALES` rather than a free-text box:
                // the valid values are a fixed set the backend already owns.
                property_type: PropertyType::Locale,
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

    #[tokio::test]
    async fn catalog_renders_with_fallback_and_interpolation() {
        let path = temp_db_path("catalog");
        let pool = Arc::new(crate::db::init_system_pool(&path).await.unwrap());

        let bundle = |loc: &str, pairs: &[(&str, &str)]| LocaleBundle::new(
            loc,
            pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
        );
        let cat = I18nCatalog::new(Arc::clone(&pool), vec![
            bundle("en", &[("p.hi", "Hi {name}"), ("p.only_en", "Only EN")]),
            bundle("it", &[("p.hi", "Ciao {name}")]),
        ]);

        // Exact locale hit + placeholder fill.
        assert_eq!(cat.get("it", "p.hi", &[("name", "Ada")]), "Ciao Ada");
        // Missing key in locale → English fallback.
        assert_eq!(cat.get("it", "p.only_en", &[]), "Only EN");
        // Missing everywhere → the raw key.
        assert_eq!(cat.get("it", "p.absent", &[]), "p.absent");
        // Unknown locale → English fallback.
        assert_eq!(cat.get("de", "p.hi", &[("name", "Bo")]), "Hi Bo");

        pool.close().await;
        cleanup(&path);
    }
}
