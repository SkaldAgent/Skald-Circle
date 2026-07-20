//! First-run instance initialization — the seam both setup shells share.
//!
//! `skald-setup` (the terminal wizard) and the web setup endpoint both need to do
//! the same thing exactly once: seed the instance's roles from a chosen **seed
//! profile** and create the first admin. Keeping that here — rather than duplicated
//! in each shell — is what stops the two paths from drifting apart.
//!
//! A [`SeedProfile`] is the neutral primitive (§0.1); the domain flavour ("Family",
//! "Office", …) lives only in the profile's seed data — labels and role presets,
//! never in the engine. One profile ships today; adding another is data, not code.

use anyhow::{Result, anyhow};
use sqlx::SqlitePool;

use crate::db::{self, roles::ADMIN_ROLE_ID};
use crate::users::UserManager;

/// One role a profile seeds. `attrs` is the `roles.attrs` JSON (§0.1) — `ui_mode`,
/// allowed security-groups, and future role attributes.
pub struct RoleSeed {
    pub id:               &'static str,
    pub label:            &'static str,
    pub permission_group: &'static str,
    pub attrs:            Option<&'static str>,
}

/// A named preset of roles the admin picks at first-run. Neutral mechanism; the
/// domain lives in the data.
pub struct SeedProfile {
    pub id:    &'static str,
    pub label: &'static str,
    pub roles: Vec<RoleSeed>,
}

/// The profiles offered by the setup picker. `admin` is seeded universally at
/// table-creation (an FK invariant, `db::roles::seed_admin`), so a profile only
/// adds its **domain** roles. Ship one now; `office` / `family-no-kids` are just
/// more entries here — no engine change.
pub fn seed_profiles() -> Vec<SeedProfile> {
    vec![SeedProfile {
        id:    "family",
        label: "Family",
        roles: vec![
            RoleSeed {
                id:               "member",
                label:            "Member",
                permission_group: "default",
                attrs:            Some(r#"{"ui_mode":"full"}"#),
            },
            RoleSeed {
                id:               "children",
                label:            "Children",
                permission_group: "default",
                attrs:            Some(r#"{"ui_mode":"simple"}"#),
            },
        ],
    }]
}

/// Look up a profile by id.
pub fn seed_profile(id: &str) -> Option<SeedProfile> {
    seed_profiles().into_iter().find(|p| p.id == id)
}

/// Seed a profile's roles (+ their default self-service capabilities) into the
/// registry. Idempotent: an existing role id is left untouched, so a re-run never
/// clobbers an admin-edited role. Runs at first-run, after every registry table
/// exists — so `role_capabilities` is present (no ordering hazard).
pub async fn apply_seed_profile(pool: &SqlitePool, profile_id: &str) -> Result<()> {
    let profile =
        seed_profile(profile_id).ok_or_else(|| anyhow!("unknown seed profile: {profile_id}"))?;
    for role in &profile.roles {
        if db::roles::get(pool, role.id).await?.is_some() {
            continue; // already present — leave it as the admin left it
        }
        db::roles::insert(pool, role.id, role.label, role.permission_group, role.attrs).await?;
        // The standard self-service capabilities, exactly as `roles::create` grants
        // them through the API (§14).
        db::role_capabilities::seed_defaults(pool, role.id).await?;
    }
    Ok(())
}

/// Everything a shell needs to know about the first admin.
pub struct FirstAdmin<'a> {
    pub username:     &'a str,
    pub display_name: Option<&'a str>,
    pub password:     Option<&'a str>,
    pub encrypted:    bool,
    /// Interface language → the instance default (`ui_locale`). `None` leaves the
    /// registry default (English) in place.
    pub locale:       Option<&'a str>,
}

/// First-run initialization, shared by both setup shells: apply the chosen seed
/// profile, create the admin, set the instance default locale. Returns the new
/// admin's user id.
///
/// The default-locale write goes straight to `db::config` (no system bus): at
/// first-run nothing is listening, so both shells converge on the same path.
pub async fn initialize_instance(
    users:      &UserManager,
    pool:       &SqlitePool,
    profile_id: &str,
    admin:      FirstAdmin<'_>,
) -> Result<String> {
    apply_seed_profile(pool, profile_id).await?;

    let id = users
        .register_user(
            admin.username,
            admin.display_name,
            ADMIN_ROLE_ID,
            admin.password,
            admin.encrypted,
        )
        .await?;

    if let Some(locale) = admin.locale {
        crate::i18n::set_default_locale(pool, locale).await?;
    }

    Ok(id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::roles::UiMode;

    fn tmp_db(tag: &str) -> String {
        let dir = std::env::temp_dir().join(format!("skald-setup-{tag}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("system.db").to_str().unwrap().to_string()
    }

    #[tokio::test]
    async fn family_profile_seeds_roles_and_caps_idempotently() {
        let pool = crate::db::init_system_pool(&tmp_db("family")).await.unwrap();
        apply_seed_profile(&pool, "family").await.unwrap();
        apply_seed_profile(&pool, "family").await.unwrap(); // idempotent

        let member = db::roles::get(&pool, "member").await.unwrap().unwrap();
        assert_eq!(member.attrs_parsed().ui_mode, UiMode::Full);
        let children = db::roles::get(&pool, "children").await.unwrap().unwrap();
        assert_eq!(children.attrs_parsed().ui_mode, UiMode::Simple);

        // The standard self-service capabilities were granted to a seeded role.
        assert!(db::role_capabilities::has(
            &pool, "member", db::role_capabilities::REGISTER_REMOTE,
        ).await.unwrap());
    }

    #[tokio::test]
    async fn unknown_profile_is_an_error() {
        let pool = crate::db::init_system_pool(&tmp_db("unknown")).await.unwrap();
        assert!(apply_seed_profile(&pool, "does-not-exist").await.is_err());
    }
}
