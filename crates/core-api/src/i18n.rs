//! Backend localization contract shared by the core and every plugin.
//!
//! Two halves:
//! - [`LocaleBundle`] — what a plugin *declares* (its translation table for one
//!   locale), returned from `Plugin::i18n()` and collected into a single catalog
//!   at boot. Keys must be namespaced (`plugin.<id>.<key>`) so bundles from
//!   different plugins — and the core — merge without clobbering each other, and
//!   so the same key can back the frontend fragment's `t()` string.
//! - [`I18nApi`] — what a plugin *calls* at request time to turn a key into text
//!   for the caller. Injected into `PluginContext.i18n`; the concrete impl lives
//!   in `skald-core` (it owns the locale-resolution chain and the system pool).
//!
//! The core never emits user-facing text through a hardcoded English literal
//! once it can go through this seam — a plugin's own error/generated strings
//! reach the user in the user's language, mirroring the frontend `i18n.js`.

use std::collections::HashMap;

use async_trait::async_trait;

/// One namespace's translation table for a single locale, as declared by a
/// plugin (or the core). Merged into the boot-time catalog keyed by locale;
/// keys collide across bundles only if two authors reuse the same fully
/// qualified key, which the `plugin.<id>.` convention prevents.
#[derive(Debug, Clone)]
pub struct LocaleBundle {
    /// Locale code — `"en"`, `"it"`, `"fr"`. Must match a supported locale;
    /// anything else is simply never selected by the resolver.
    pub locale:  String,
    /// Fully qualified key → translated string. Placeholders are `{name}`.
    pub strings: HashMap<String, String>,
}

impl LocaleBundle {
    pub fn new(locale: impl Into<String>, strings: HashMap<String, String>) -> Self {
        Self { locale: locale.into(), strings }
    }
}

/// Runtime translation, injected into [`crate::plugin::PluginContext`].
///
/// The catalog behind it is built once at boot from every plugin's
/// `Plugin::i18n()`. Resolution mirrors the rest of the system: a user's
/// `users.locale` override → the instance default → built-in English → the raw
/// key as a last resort. Placeholders (`{name}`) are filled from `args`.
#[async_trait]
pub trait I18nApi: Send + Sync {
    /// Translate `key` for `user_id`, resolving *their* effective locale. Use
    /// this from any request/notification path where the target user is known.
    async fn for_user(&self, user_id: &str, key: &str, args: &[(&str, &str)]) -> String;

    /// Translate for an already-resolved locale — for contexts with no single
    /// user (boot logs, broadcast copy) that have decided a locale by other means.
    fn get(&self, locale: &str, key: &str, args: &[(&str, &str)]) -> String;
}
