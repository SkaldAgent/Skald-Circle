use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

/// The built-in admin role — immutable from the API.
pub const ADMIN_ROLE_ID: &str = "admin";

#[derive(Debug, Clone, Serialize)]
pub struct Role {
    pub id:               String,
    pub label:            String,
    pub permission_group: String,
    pub attrs:            Option<String>,
    pub created_at:       String,
}

type RawRow = (String, String, String, Option<String>, String);

fn from_raw((id, label, permission_group, attrs, created_at): RawRow) -> Role {
    Role { id, label, permission_group, attrs, created_at }
}

// ── Typed view over `roles.attrs` (§0.1: role attributes live in free-form JSON,
// never per-attribute columns) ────────────────────────────────────────────────

/// Interface mode a role opts into. `full` unless the role explicitly chooses the
/// simplified UI; `admin` is resolved to `full` upstream. Values other than the two
/// known ones fall back to `full` (tolerant parse).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum UiMode {
    #[default]
    Full,
    Simple,
}

impl UiMode {
    pub fn as_str(self) -> &'static str {
        match self {
            UiMode::Full => "full",
            UiMode::Simple => "simple",
        }
    }
}

/// Typed parse of `roles.attrs`. The **single** place that reads the attrs JSON, so
/// scattered `serde_json::Value.get(...)` calls don't drift. Tolerant: any parse
/// error or missing key yields defaults.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct RoleAttrs {
    pub ui_mode: UiMode,
    /// Security-groups (`tool_permission_groups` ids) this role may use **in addition**
    /// to its default `permission_group`. The default is always implicitly allowed; the
    /// effective set is `unique({permission_group} ∪ permission_groups)`.
    pub permission_groups: Vec<String>,
    /// The entry (`type:chat`) agent members of this role talk to by default — e.g.
    /// `children` → `kid` (Companion), `member`/`admin` → `assistant`. Resolved into the
    /// per-user runtime at login (see [`default_chat_agent_for_user`]). `None` (or absent
    /// attrs) falls back to [`crate::agents::DEFAULT_CHAT_AGENT`]. Data-driven, not an
    /// enum (§0.1): a future per-user override layers on top of this.
    pub chat_agent: Option<String>,
}

impl RoleAttrs {
    pub fn from_opt(attrs: &Option<String>) -> RoleAttrs {
        attrs
            .as_deref()
            .and_then(|s| serde_json::from_str(s).ok())
            .unwrap_or_default()
    }
}

impl Role {
    pub fn attrs_parsed(&self) -> RoleAttrs {
        RoleAttrs::from_opt(&self.attrs)
    }

    /// The security-groups this role may select: its default first, then any extras
    /// from `attrs.permission_groups`, deduped.
    pub fn effective_groups(&self) -> Vec<String> {
        let mut out = vec![self.permission_group.clone()];
        for g in self.attrs_parsed().permission_groups {
            if !out.contains(&g) {
                out.push(g);
            }
        }
        out
    }
}

/// The entry chat agent for a user: their role's `attrs.chat_agent`, else the neutral
/// [`crate::agents::DEFAULT_CHAT_AGENT`]. The single resolver behind the per-user hub
/// default and `provisioning_for_source`, so every session-creation path agrees on which
/// agent a member lands on. Tolerant by construction — a missing user, missing role, or
/// unset `chat_agent` all collapse to the default rather than erroring (a chat must open).
///
/// A future per-user override would slot in here, checked before the role default.
pub async fn default_chat_agent_for_user(pool: &SqlitePool, user_id: &str) -> String {
    let role_agent = async {
        let user = super::users::get(pool, user_id).await.ok()??;
        let role = get(pool, &user.role_id).await.ok()??;
        role.attrs_parsed().chat_agent.filter(|s| !s.trim().is_empty())
    }
    .await;
    role_agent.unwrap_or_else(|| crate::agents::DEFAULT_CHAT_AGENT.to_string())
}

/// Whether a role may use `group_id` as its session security-group. `admin` holds
/// every group by construction; a missing role allows nothing.
pub async fn role_allows_group(pool: &SqlitePool, role_id: &str, group_id: &str) -> Result<bool> {
    if role_id == ADMIN_ROLE_ID {
        return Ok(true);
    }
    match get(pool, role_id).await? {
        Some(role) => Ok(role.effective_groups().iter().any(|g| g == group_id)),
        None => Ok(false),
    }
}

// ── Reads ────────────────────────────────────────────────────────────────────

pub async fn list(pool: &SqlitePool) -> Result<Vec<Role>> {
    let rows = sqlx::query_as::<_, RawRow>(
        "SELECT id, label, permission_group, attrs, created_at
         FROM   roles
         ORDER  BY label",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(from_raw).collect())
}

pub async fn get(pool: &SqlitePool, id: &str) -> Result<Option<Role>> {
    let row = sqlx::query_as::<_, RawRow>(
        "SELECT id, label, permission_group, attrs, created_at
         FROM   roles WHERE id = ?",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(from_raw))
}

// ── Writes ───────────────────────────────────────────────────────────────────

pub async fn insert(
    pool:             &SqlitePool,
    id:               &str,
    label:            &str,
    permission_group: &str,
    attrs:            Option<&str>,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO roles (id, label, permission_group, attrs)
         VALUES (?, ?, ?, ?)",
    )
    .bind(id)
    .bind(label)
    .bind(permission_group)
    .bind(attrs)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn update(
    pool:             &SqlitePool,
    id:               &str,
    label:            &str,
    permission_group: &str,
    attrs:            Option<&str>,
) -> Result<bool> {
    let rows = sqlx::query(
        "UPDATE roles SET label = ?, permission_group = ?, attrs = ? WHERE id = ?",
    )
    .bind(label)
    .bind(permission_group)
    .bind(attrs)
    .bind(id)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(rows > 0)
}

pub async fn delete(pool: &SqlitePool, id: &str) -> Result<()> {
    if id == ADMIN_ROLE_ID {
        bail!("cannot delete the built-in admin role");
    }
    let rows = sqlx::query("DELETE FROM roles WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?
        .rows_affected();
    if rows == 0 {
        bail!("no such role: {id}");
    }
    Ok(())
}

/// How many users are assigned to a role — prevents deletion when non-zero.
pub async fn user_count(pool: &SqlitePool, role_id: &str) -> Result<i64> {
    let (n,) = sqlx::query_as::<_, (i64,)>("SELECT COUNT(*) FROM users WHERE role_id = ?")
        .bind(role_id)
        .fetch_one(pool)
        .await?;
    Ok(n)
}

// ── Seed ─────────────────────────────────────────────────────────────────────

/// Inserts the built-in `admin` role. Idempotent.
pub async fn seed_admin(pool: &SqlitePool) -> Result<()> {
    // `chat_agent` is explicit so the role editor shows admin's default rather than an
    // empty pill; the fallback in `default_chat_agent_for_user` would resolve the same.
    sqlx::query(
        r#"INSERT OR IGNORE INTO roles (id, label, permission_group, attrs)
         VALUES ('admin', 'Administrator', 'default', '{"chat_agent":"assistant"}')"#,
    )
    .execute(pool)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_db(tag: &str) -> String {
        let dir = std::env::temp_dir().join(format!("skald-roles-{tag}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("system.db").to_str().unwrap().to_string()
    }

    fn role(permission_group: &str, attrs: Option<&str>) -> Role {
        Role {
            id:               "member".into(),
            label:            "Member".into(),
            permission_group: permission_group.into(),
            attrs:            attrs.map(str::to_string),
            created_at:       String::new(),
        }
    }

    #[test]
    fn role_attrs_are_tolerant() {
        // Missing → defaults.
        let a = RoleAttrs::from_opt(&None);
        assert_eq!(a.ui_mode, UiMode::Full);
        assert!(a.permission_groups.is_empty());
        assert!(a.chat_agent.is_none());

        // Populated.
        let a = RoleAttrs::from_opt(&Some(
            r#"{"ui_mode":"simple","permission_groups":["ops","research"],"chat_agent":"kid"}"#.into(),
        ));
        assert_eq!(a.ui_mode, UiMode::Simple);
        assert_eq!(a.permission_groups, vec!["ops", "research"]);
        assert_eq!(a.chat_agent.as_deref(), Some("kid"));

        // Malformed JSON → defaults, never an error.
        let a = RoleAttrs::from_opt(&Some("not json".into()));
        assert_eq!(a.ui_mode, UiMode::Full);
        assert!(a.permission_groups.is_empty());
    }

    #[test]
    fn effective_groups_prepends_default_and_dedups() {
        let r = role("default", Some(r#"{"permission_groups":["ops","default","research"]}"#));
        assert_eq!(r.effective_groups(), vec!["default", "ops", "research"]);

        // No extras → just the default.
        let r = role("kids", None);
        assert_eq!(r.effective_groups(), vec!["kids"]);
    }

    #[tokio::test]
    async fn role_allows_group_admin_member_and_unknown() {
        let pool = crate::db::init_system_pool(&tmp_db("allows")).await.unwrap();

        // admin is seeded by init and allows any group by construction.
        assert!(role_allows_group(&pool, ADMIN_ROLE_ID, "anything").await.unwrap());

        insert(&pool, "member", "Member", "default", Some(r#"{"permission_groups":["ops"]}"#))
            .await
            .unwrap();
        assert!(role_allows_group(&pool, "member", "default").await.unwrap());
        assert!(role_allows_group(&pool, "member", "ops").await.unwrap());
        assert!(!role_allows_group(&pool, "member", "research").await.unwrap());

        // An unknown role allows nothing.
        assert!(!role_allows_group(&pool, "ghost", "default").await.unwrap());
    }

    #[tokio::test]
    async fn default_chat_agent_resolves_role_then_falls_back() {
        let pool = crate::db::init_system_pool(&tmp_db("chatagent")).await.unwrap();

        // A role with an explicit chat_agent, and one without.
        insert(&pool, "children", "Children", "default", Some(r#"{"chat_agent":"kid"}"#))
            .await.unwrap();
        insert(&pool, "member", "Member", "default", Some(r#"{"ui_mode":"full"}"#))
            .await.unwrap();

        // Minimal cleartext user rows (encrypted=0, no credentials) — bypasses Argon2.
        for (id, uname, role) in [("u_kid", "kid1", "children"), ("u_adult", "adult1", "member")] {
            sqlx::query("INSERT INTO users (id, username, role_id, encrypted) VALUES (?, ?, ?, 0)")
                .bind(id).bind(uname).bind(role).execute(&pool).await.unwrap();
        }

        // Role's chat_agent wins; a role without one falls back to the neutral default;
        // an unknown user also falls back (never errors — a chat must be able to open).
        assert_eq!(default_chat_agent_for_user(&pool, "u_kid").await, "kid");
        assert_eq!(default_chat_agent_for_user(&pool, "u_adult").await, crate::agents::DEFAULT_CHAT_AGENT);
        assert_eq!(default_chat_agent_for_user(&pool, "ghost").await, crate::agents::DEFAULT_CHAT_AGENT);
    }
}
