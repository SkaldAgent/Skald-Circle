//! `users` — user directory and auth material.
//!
//! This table lives in the system DB, which anyone owning the box can read. So
//! it must never store anything from which a user's key can be derived.
//!
//! For an **encrypted** user it holds the DEK *wrapped* under a key derived from
//! the password: useless without the password, and the wrap's AEAD tag doubles
//! as the password verifier. That is why an encrypted user has no
//! `password_hash` — a second hash of the same password would only hand an
//! offline attacker an easier target than the wrap itself.
//!
//! A **cleartext** user has no DB key to bind a verifier to, so it carries an
//! ordinary Argon2id hash instead (harmless: that DB is readable anyway).
//!
//! [`Credentials`] makes the two shapes mutually exclusive in the type system,
//! mirroring the `CHECK` constraint on the table.

use anyhow::{Result, anyhow, bail};
use serde::Serialize;
use sqlx::SqlitePool;

/// KDF settings as JSON, e.g. `{"algo":"argon2id","m":65536,"t":3,"p":1}`.
/// Not secret — calibrated on the box when the user is created.
pub type KdfParams = String;

/// Argon2id verifier for a user whose database is not encrypted.
#[derive(Clone)]
pub struct ClearVerifier {
    pub kdf_params:    KdfParams,
    pub kdf_salt:      Vec<u8>,
    pub password_hash: Vec<u8>,
}

/// Auth material for a user. The variants mirror the table's `CHECK`: an
/// encrypted user has a wrapped DEK and no hash; a cleartext user has no
/// wrapped DEK, and may have no verifier at all (a role that cannot log in).
#[derive(Clone)]
pub enum Credentials {
    Encrypted {
        kdf_params: KdfParams,
        kdf_salt:   Vec<u8>,
        /// DEK sealed with an AEAD under `KDF(password, kdf_salt)`. Changing the
        /// password re-wraps this value; the database itself is never re-encrypted.
        database_password: Vec<u8>,
    },
    Cleartext(Option<ClearVerifier>),
}

impl Credentials {
    pub fn is_encrypted(&self) -> bool {
        matches!(self, Credentials::Encrypted { .. })
    }
}

/// A row of `users`.
///
/// Deliberately **not** `Serialize`: it carries the wrapped DEK and the password
/// verifier, and this type must never be handed to an HTTP handler by accident.
/// Use [`User::summary`] for anything that leaves the process.
#[derive(Clone)]
pub struct User {
    pub id:           String,
    pub username:     String,
    pub display_name: Option<String>,
    pub role_id:      String,
    pub credentials:  Credentials,
    pub active:       bool,
    pub created_at:   String,
    pub updated_at:   String,
}

/// The public-safe projection of a [`User`] — no key material.
#[derive(Debug, Clone, Serialize)]
pub struct UserSummary {
    pub id:           String,
    pub username:     String,
    pub display_name: Option<String>,
    pub role_id:      String,
    pub encrypted:    bool,
    pub active:       bool,
    pub created_at:   String,
    pub updated_at:   String,
}

impl User {
    pub fn is_encrypted(&self) -> bool {
        self.credentials.is_encrypted()
    }

    pub fn summary(&self) -> UserSummary {
        UserSummary {
            id:           self.id.clone(),
            username:     self.username.clone(),
            display_name: self.display_name.clone(),
            role_id:      self.role_id.clone(),
            encrypted:    self.is_encrypted(),
            active:       self.active,
            created_at:   self.created_at.clone(),
            updated_at:   self.updated_at.clone(),
        }
    }
}

// Hand-written so a stray `{:?}` — in a tracing span, an error context, a panic
// message — cannot print key material.
impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Credentials::Encrypted { .. }  => f.write_str("Encrypted(<redacted>)"),
            Credentials::Cleartext(None)   => f.write_str("Cleartext(no verifier)"),
            Credentials::Cleartext(Some(_)) => f.write_str("Cleartext(<redacted>)"),
        }
    }
}

impl std::fmt::Debug for User {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("User")
            .field("id",           &self.id)
            .field("username",     &self.username)
            .field("display_name", &self.display_name)
            .field("role_id",      &self.role_id)
            .field("credentials",  &self.credentials)
            .field("active",       &self.active)
            .finish()
    }
}

// ── Row mapping ───────────────────────────────────────────────────────────────

#[derive(sqlx::FromRow)]
struct Row {
    id:                String,
    username:          String,
    display_name:      Option<String>,
    role_id:           String,
    encrypted:         bool,
    kdf_params:        Option<String>,
    kdf_salt:          Option<Vec<u8>>,
    database_password: Option<Vec<u8>>,
    password_hash:     Option<Vec<u8>>,
    active:            bool,
    created_at:        String,
    updated_at:        String,
}

/// Builds a `&'static str` (sqlx rejects runtime-built SQL) while keeping the
/// column list — which `Row`'s `FromRow` mirrors — in exactly one place.
macro_rules! select {
    ($tail:literal) => {
        concat!(
            "SELECT id, username, display_name, role_id, encrypted, kdf_params, kdf_salt, ",
            "database_password, password_hash, active, created_at, updated_at FROM users ",
            $tail
        )
    };
}

impl TryFrom<Row> for User {
    type Error = anyhow::Error;

    fn try_from(r: Row) -> Result<Self> {
        let broken = |what: &str| anyhow!("users row {}: {what}", r.id);
        let credentials = if r.encrypted {
            Credentials::Encrypted {
                kdf_params:        r.kdf_params.ok_or_else(|| broken("encrypted without kdf_params"))?,
                kdf_salt:          r.kdf_salt.ok_or_else(|| broken("encrypted without kdf_salt"))?,
                database_password: r.database_password
                    .ok_or_else(|| broken("encrypted without database_password"))?,
            }
        } else {
            match r.password_hash {
                None => Credentials::Cleartext(None),
                Some(password_hash) => Credentials::Cleartext(Some(ClearVerifier {
                    kdf_params: r.kdf_params.ok_or_else(|| broken("verifier without kdf_params"))?,
                    kdf_salt:   r.kdf_salt.ok_or_else(|| broken("verifier without kdf_salt"))?,
                    password_hash,
                })),
            }
        };
        Ok(User {
            id:           r.id,
            username:     r.username,
            display_name: r.display_name,
            role_id:      r.role_id,
            credentials,
            active:       r.active,
            created_at:   r.created_at,
            updated_at:   r.updated_at,
        })
    }
}

/// The four credential columns, in table order.
type CredColumns<'a> = (bool, Option<&'a str>, Option<&'a [u8]>, Option<&'a [u8]>, Option<&'a [u8]>);

fn columns(c: &Credentials) -> CredColumns<'_> {
    match c {
        Credentials::Encrypted { kdf_params, kdf_salt, database_password } => (
            true,
            Some(kdf_params.as_str()),
            Some(kdf_salt.as_slice()),
            Some(database_password.as_slice()),
            None,
        ),
        Credentials::Cleartext(None) => (false, None, None, None, None),
        Credentials::Cleartext(Some(v)) => (
            false,
            Some(v.kdf_params.as_str()),
            Some(v.kdf_salt.as_slice()),
            None,
            Some(v.password_hash.as_slice()),
        ),
    }
}

// ── Reads ─────────────────────────────────────────────────────────────────────

pub async fn get(pool: &SqlitePool, id: &str) -> Result<Option<User>> {
    let row = sqlx::query_as::<_, Row>(select!("WHERE id = ?1"))
        .bind(id)
        .fetch_optional(pool)
        .await?;
    row.map(User::try_from).transpose()
}

/// Login entry point: `username` is the handle, `id` is opaque and stable.
pub async fn by_username(pool: &SqlitePool, username: &str) -> Result<Option<User>> {
    let row = sqlx::query_as::<_, Row>(select!("WHERE username = ?1"))
        .bind(username)
        .fetch_optional(pool)
        .await?;
    row.map(User::try_from).transpose()
}

pub async fn list(pool: &SqlitePool) -> Result<Vec<User>> {
    let rows = sqlx::query_as::<_, Row>(select!("ORDER BY username"))
        .fetch_all(pool)
        .await?;
    rows.into_iter().map(User::try_from).collect()
}

pub async fn count(pool: &SqlitePool) -> Result<i64> {
    let (n,) = sqlx::query_as::<_, (i64,)>("SELECT COUNT(*) FROM users")
        .fetch_one(pool)
        .await?;
    Ok(n)
}

// ── Writes ────────────────────────────────────────────────────────────────────

/// `id` is supplied by the caller and must be opaque (never the username), so a
/// rename never has to touch `database/{id}.db`.
pub async fn insert(
    pool:         &SqlitePool,
    id:           &str,
    username:     &str,
    display_name: Option<&str>,
    role_id:      &str,
    credentials:  &Credentials,
) -> Result<()> {
    let (encrypted, kdf_params, kdf_salt, database_password, password_hash) = columns(credentials);
    sqlx::query(
        "INSERT INTO users
             (id, username, display_name, role_id, encrypted,
              kdf_params, kdf_salt, database_password, password_hash)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
    )
    .bind(id)
    .bind(username)
    .bind(display_name)
    .bind(role_id)
    .bind(encrypted)
    .bind(kdf_params)
    .bind(kdf_salt)
    .bind(database_password)
    .bind(password_hash)
    .execute(pool)
    .await?;
    Ok(())
}

/// Replaces the auth material in one statement.
///
/// This is both "change password" (re-wrap the same DEK under a key derived from
/// the new password — the database is never re-encrypted) and the encrypted ↔
/// cleartext migration, since the variant carries the new shape.
pub async fn set_credentials(pool: &SqlitePool, id: &str, credentials: &Credentials) -> Result<()> {
    let (encrypted, kdf_params, kdf_salt, database_password, password_hash) = columns(credentials);
    let n = sqlx::query(
        "UPDATE users SET
             encrypted         = ?2,
             kdf_params        = ?3,
             kdf_salt          = ?4,
             database_password = ?5,
             password_hash     = ?6,
             updated_at        = datetime('now')
         WHERE id = ?1",
    )
    .bind(id)
    .bind(encrypted)
    .bind(kdf_params)
    .bind(kdf_salt)
    .bind(database_password)
    .bind(password_hash)
    .execute(pool)
    .await?
    .rows_affected();
    if n == 0 {
        bail!("no such user: {id}");
    }
    Ok(())
}

pub async fn set_active(pool: &SqlitePool, id: &str, active: bool) -> Result<()> {
    let n = sqlx::query(
        "UPDATE users SET active = ?2, updated_at = datetime('now') WHERE id = ?1",
    )
    .bind(id)
    .bind(active)
    .execute(pool)
    .await?
    .rows_affected();
    if n == 0 {
        bail!("no such user: {id}");
    }
    Ok(())
}

pub async fn rename(pool: &SqlitePool, id: &str, username: &str, display_name: Option<&str>) -> Result<()> {
    let n = sqlx::query(
        "UPDATE users SET username = ?2, display_name = ?3, updated_at = datetime('now')
         WHERE id = ?1",
    )
    .bind(id)
    .bind(username)
    .bind(display_name)
    .execute(pool)
    .await?
    .rows_affected();
    if n == 0 {
        bail!("no such user: {id}");
    }
    Ok(())
}

/// Removes the directory row only. The caller still owns `database/{id}.db`:
/// erasing a user means deleting that file too.
pub async fn delete(pool: &SqlitePool, id: &str) -> Result<()> {
    sqlx::query("DELETE FROM users WHERE id = ?1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Nested on purpose: also covers `init_system_pool` creating the parent directory.
    fn temp_db_path(tag: &str) -> String {
        let mut p = std::env::temp_dir();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        p.push(format!("skald-test-{tag}-{}-{nanos}", std::process::id()));
        p.push("database");
        p.push("system.db");
        p.to_string_lossy().into_owned()
    }

    fn cleanup(path: &str) {
        if let Some(dir) = std::path::Path::new(path).parent().and_then(|p| p.parent()) {
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    fn encrypted() -> Credentials {
        Credentials::Encrypted {
            kdf_params:        r#"{"algo":"argon2id","m":65536,"t":3,"p":1}"#.into(),
            kdf_salt:          vec![1, 2, 3, 4],
            database_password: vec![0xDE, 0xAD, 0xBE, 0xEF],
        }
    }

    fn cleartext() -> Credentials {
        Credentials::Cleartext(Some(ClearVerifier {
            kdf_params:    r#"{"algo":"argon2id","m":65536,"t":3,"p":1}"#.into(),
            kdf_salt:      vec![5, 6, 7, 8],
            password_hash: vec![0xAB, 0xCD],
        }))
    }

    #[tokio::test]
    async fn init_system_pool_creates_the_database_directory() {
        let path = temp_db_path("users-mkdir");
        assert!(!std::path::Path::new(&path).parent().unwrap().exists());

        let pool = crate::db::init_system_pool(&path).await.unwrap();
        assert!(std::path::Path::new(&path).exists(), "system.db must exist under a fresh database/");

        pool.close().await;
        cleanup(&path);
    }

    #[tokio::test]
    async fn encrypted_user_round_trips_and_keeps_the_wrapped_dek() {
        let path = temp_db_path("users-enc");
        let pool = crate::db::init_system_pool(&path).await.unwrap();

        insert(&pool, "u-1", "ada", Some("Ada"), "admin", &encrypted()).await.unwrap();

        let u = by_username(&pool, "ada").await.unwrap().expect("user by username");
        assert_eq!(u.id, "u-1");
        assert!(u.is_encrypted());
        assert!(u.active);
        match u.credentials {
            Credentials::Encrypted { database_password, kdf_salt, .. } => {
                assert_eq!(database_password, vec![0xDE, 0xAD, 0xBE, 0xEF]);
                assert_eq!(kdf_salt, vec![1, 2, 3, 4]);
            }
            other => panic!("expected Encrypted, got {other:?}"),
        }

        pool.close().await;
        cleanup(&path);
    }

    #[tokio::test]
    async fn cleartext_user_round_trips_with_and_without_a_verifier() {
        let path = temp_db_path("users-clear");
        let pool = crate::db::init_system_pool(&path).await.unwrap();

        insert(&pool, "u-1", "kid", None, "children", &cleartext()).await.unwrap();
        insert(&pool, "u-2", "kiosk", None, "children", &Credentials::Cleartext(None)).await.unwrap();

        let with = get(&pool, "u-1").await.unwrap().unwrap();
        assert!(!with.is_encrypted());
        match with.credentials {
            Credentials::Cleartext(Some(v)) => assert_eq!(v.password_hash, vec![0xAB, 0xCD]),
            other => panic!("expected a verifier, got {other:?}"),
        }

        let without = get(&pool, "u-2").await.unwrap().unwrap();
        assert!(matches!(without.credentials, Credentials::Cleartext(None)));

        assert_eq!(count(&pool).await.unwrap(), 2);
        assert_eq!(list(&pool).await.unwrap().len(), 2);

        pool.close().await;
        cleanup(&path);
    }

    /// Changing the password re-wraps the DEK; migrating to cleartext must clear it.
    #[tokio::test]
    async fn set_credentials_rewraps_and_migrates() {
        let path = temp_db_path("users-rewrap");
        let pool = crate::db::init_system_pool(&path).await.unwrap();

        insert(&pool, "u-1", "ada", None, "admin", &encrypted()).await.unwrap();

        let rewrapped = Credentials::Encrypted {
            kdf_params:        r#"{"algo":"argon2id","m":65536,"t":3,"p":1}"#.into(),
            kdf_salt:          vec![9, 9, 9],
            database_password: vec![0xFE, 0xED],
        };
        set_credentials(&pool, "u-1", &rewrapped).await.unwrap();
        match get(&pool, "u-1").await.unwrap().unwrap().credentials {
            Credentials::Encrypted { database_password, .. } => assert_eq!(database_password, vec![0xFE, 0xED]),
            other => panic!("expected Encrypted, got {other:?}"),
        }

        set_credentials(&pool, "u-1", &cleartext()).await.unwrap();
        let u = get(&pool, "u-1").await.unwrap().unwrap();
        assert!(!u.is_encrypted(), "migrating must flip `encrypted` and drop the wrapped DEK");

        assert!(set_credentials(&pool, "ghost", &cleartext()).await.is_err(), "unknown id must fail");

        pool.close().await;
        cleanup(&path);
    }

    /// The SQL `CHECK` is the last line of defence when a row is written without
    /// going through [`Credentials`].
    #[tokio::test]
    async fn check_constraint_rejects_impossible_rows() {
        let path = temp_db_path("users-check");
        let pool = crate::db::init_system_pool(&path).await.unwrap();

        // encrypted without a wrapped DEK
        let err = sqlx::query(
            "INSERT INTO users (id, username, role_id, encrypted) VALUES ('x', 'x', 'admin', 1)",
        )
        .execute(&pool)
        .await;
        assert!(err.is_err(), "encrypted=1 requires database_password");

        // encrypted *and* carrying a password hash
        let err = sqlx::query(
            "INSERT INTO users (id, username, role_id, encrypted, database_password, password_hash)
             VALUES ('y', 'y', 'admin', 1, X'00', X'01')",
        )
        .execute(&pool)
        .await;
        assert!(err.is_err(), "an encrypted user must not also store a password hash");

        // cleartext carrying a wrapped DEK
        let err = sqlx::query(
            "INSERT INTO users (id, username, role_id, encrypted, database_password)
             VALUES ('z', 'z', 'admin', 0, X'00')",
        )
        .execute(&pool)
        .await;
        assert!(err.is_err(), "cleartext=0 must not store a wrapped DEK");

        assert_eq!(count(&pool).await.unwrap(), 0);

        pool.close().await;
        cleanup(&path);
    }

    #[tokio::test]
    async fn debug_never_prints_key_material() {
        let u = User {
            id:           "u-1".into(),
            username:     "ada".into(),
            display_name: None,
            role_id:      "admin".into(),
            credentials:  encrypted(),
            active:       true,
            created_at:   "now".into(),
            updated_at:   "now".into(),
        };
        let printed = format!("{u:?}");
        assert!(printed.contains("ada"));
        assert!(!printed.contains("222"), "no raw DEK bytes");
        assert!(!printed.contains("deadbeef") && !printed.contains("DEADBEEF"));
        assert!(printed.contains("<redacted>"));
    }
}
