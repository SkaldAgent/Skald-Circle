//! Backend translation bundles for the mobile-connector.
//!
//! These are the plugin's **backend** strings — the error/response text its
//! router returns, resolved to the caller's language via `PluginContext.i18n`
//! (see `core_api::i18n`). The frontend fragment's UI strings live separately in
//! `web/i18n.js` (registered client-side); the two sets barely overlap, so each
//! side owns its own table rather than sharing one over an endpoint.
//!
//! The tables ship as JSON embedded at compile time — one file per locale, keys
//! namespaced `plugin.mobile-connector.*`. A malformed file is skipped (its
//! locale simply falls back to English) rather than failing the build path.

use std::collections::HashMap;

use core_api::i18n::LocaleBundle;

/// Every locale bundle this plugin contributes, parsed from the embedded JSON.
pub fn bundles() -> Vec<LocaleBundle> {
    [
        ("en", include_str!("../i18n/en.json")),
        ("it", include_str!("../i18n/it.json")),
        ("fr", include_str!("../i18n/fr.json")),
    ]
    .into_iter()
    .filter_map(|(locale, raw)| {
        match serde_json::from_str::<HashMap<String, String>>(raw) {
            Ok(strings) => Some(LocaleBundle::new(locale, strings)),
            Err(e) => {
                tracing::warn!(locale, error = %e, "mobile-connector i18n bundle failed to parse");
                None
            }
        }
    })
    .collect()
}
