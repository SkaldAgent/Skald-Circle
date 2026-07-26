//! `UserManager` — the user directory, authentication, and the registry of
//! unlocked databases (blueprint §9 / §11).
//!
//! It owns the registry pool (`system.db`) and a map `userid → SqlitePool` of the
//! per-user databases that are currently **unlocked**.
//!
//! The pool *is* the unlock token. Its connect options carry the DEK as
//! SQLCipher's raw key, so an open pool means the key is in RAM and a dropped
//! pool means the database is locked again. There is no separate key registry to
//! keep in sync, and §9's lifetime — unlocked from first login until the process
//! restarts — falls out of the map's lifetime.
//!
//! Three responsibilities live here on purpose: directory CRUD, credential
//! checking, and pool lifecycle. The seam for splitting them (`UserDirectory` /
//! `UserVault`) is the `Credentials` boundary, if this ever grows.
//!
//! **Boundary**: this is credential-check plus pool lifecycle. Whatever maps an
//! HTTP session or a token to a user id sits above it. `UserManager` knows
//! nothing about cookies.

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use anyhow::{Result, anyhow, bail};
use sqlx::SqlitePool;
use tracing::{info, warn};

use crate::crypto::{self, Dek, KdfParams, KeyError};
use crate::db::{self, DATABASE_DIR};
use crate::db::users::{ClearVerifier, Credentials, User, UserSummary};

/// Why a login did not happen. Deliberately not `anyhow`: the caller has to be
/// able to tell "wrong password" from "the file is gone", and answer the two very
/// differently.
#[derive(Debug)]
pub enum AuthError {
    UnknownUser,
    Inactive,
    /// The AEAD tag on the sealed DEK did not verify, or the stored verifier did
    /// not match. One answer for both, so nothing distinguishes them from outside.
    WrongPassword,
    /// The user has credentials but none were supplied.
    PasswordRequired,
    /// The user has no verifier at all; offering a password is a caller bug.
    PasswordNotSet,
    /// The row exists and the password was right, but `{userid}.db` is not there.
    /// Never recreated: a fresh empty database under the correct password would
    /// hide the loss instead of reporting it.
    MissingDatabase(PathBuf),
    Internal(anyhow::Error),
}

impl fmt::Display for AuthError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AuthError::UnknownUser => f.write_str("no such user"),
            AuthError::Inactive => f.write_str("user is not active"),
            AuthError::WrongPassword => f.write_str("wrong password"),
            AuthError::PasswordRequired => f.write_str("this user requires a password"),
            AuthError::PasswordNotSet => f.write_str("this user has no password"),
            AuthError::MissingDatabase(p) => {
                write!(f, "database file is missing: {}", p.display())
            }
            AuthError::Internal(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for AuthError {}

impl From<anyhow::Error> for AuthError {
    fn from(e: anyhow::Error) -> Self {
        AuthError::Internal(e)
    }
}

impl From<sqlx::Error> for AuthError {
    fn from(e: sqlx::Error) -> Self {
        AuthError::Internal(e.into())
    }
}

pub struct UserManager {
    /// `system.db`: the directory, read before anything else can be opened.
    system: Arc<SqlitePool>,
    /// The unlocked databases. A `std::sync::RwLock` on purpose — holding it
    /// across an `.await` would make the future `!Send` and stop compiling, which
    /// is the enforcement we want.
    unlocked: RwLock<HashMap<String, SqlitePool>>,
    /// Where `{userid}.db` files live, and the KDF cost applied to new users.
    /// Both are fields rather than constants so tests need neither the process's
    /// working directory nor a real 256 MiB derivation.
    db_dir: PathBuf,
    kdf: KdfParams,
}

impl UserManager {
    pub fn new(system: Arc<SqlitePool>) -> Self {
        Self {
            system,
            unlocked: RwLock::new(HashMap::new()),
            db_dir: PathBuf::from(DATABASE_DIR),
            kdf: KdfParams::default(),
        }
    }

    fn path_of(&self, id: &str) -> PathBuf {
        db::user_db_path(&self.db_dir, id)
    }

    /// The registry pool. Not a user's database — the directory everything else
    /// is looked up in.
    pub fn system(&self) -> &SqlitePool {
        &self.system
    }

    // ── Directory reads ───────────────────────────────────────────────────────

    pub async fn list(&self) -> Result<Vec<UserSummary>> {
        Ok(db::users::list(&self.system).await?.iter().map(User::summary).collect())
    }

    pub async fn get(&self, id: &str) -> Result<Option<UserSummary>> {
        Ok(db::users::get(&self.system, id).await?.map(|u| u.summary()))
    }

    pub async fn by_username(&self, username: &str) -> Result<Option<UserSummary>> {
        Ok(db::users::by_username(&self.system, username).await?.map(|u| u.summary()))
    }

    pub async fn count(&self) -> Result<i64> {
        db::users::count(&self.system).await
    }

    /// Verifies the password **always**, regardless of whether the pool is
    /// already unlocked. Use this for login authentication; use [`open_db`]
    /// only for pool lifecycle.
    pub async fn verify_credentials(&self, id: &str, password: &str) -> Result<(), AuthError> {
        let user = db::users::get(&self.system, id)
            .await
            .map_err(AuthError::Internal)?
            .ok_or(AuthError::UnknownUser)?;
        if !user.active {
            return Err(AuthError::Inactive);
        }
        self.authenticate(&user, Some(password)).await?;
        Ok(())
    }

    // ── Unlock registry ───────────────────────────────────────────────────────

    /// The user's pool, or `None` when the database is still locked (§9).
    /// Returns a clone: `SqlitePool` is an `Arc` internally, so this is cheap.
    pub fn pool_of(&self, id: &str) -> Option<SqlitePool> {
        self.unlocked.read().ok()?.get(id).cloned()
    }

    pub fn is_unlocked(&self, id: &str) -> bool {
        self.unlocked.read().map(|m| m.contains_key(id)).unwrap_or(false)
    }

    /// Login and unlock in one operation.
    ///
    /// For an encrypted user a single Argon2id pass answers both questions: the
    /// sealed DEK either opens — password right, and here is the key — or its
    /// AEAD tag fails, which is the wrong password. There is no second hash to
    /// check, and none to steal from `system.db`.
    ///
    /// Idempotent: an already-unlocked user gets the existing pool back without
    /// re-deriving anything.
    ///
    /// Note that an unknown user answers immediately while a real one costs a
    /// full derivation. Under the threat model (§2) the adversary owns the box
    /// and can simply read `users`, so this timing difference buys nothing; a
    /// public login endpoint would want to think about it again.
    pub async fn open_db(&self, id: &str, password: Option<&str>) -> Result<SqlitePool, AuthError> {
        if let Some(pool) = self.pool_of(id) {
            return Ok(pool);
        }

        let user = db::users::get(&self.system, id)
            .await
            .map_err(AuthError::Internal)?
            .ok_or(AuthError::UnknownUser)?;

        if !user.active {
            return Err(AuthError::Inactive);
        }

        let key = self.authenticate(&user, password).await?;

        // The row exists, so the file must too — `register_user` writes it first.
        let path = self.path_of(id);
        if !path.exists() {
            return Err(AuthError::MissingDatabase(path));
        }

        let pool = db::open_user_pool(&path, key.as_ref())
            .await
            .map_err(AuthError::Internal)?;

        // Another task may have unlocked the same user while we were deriving.
        // Whoever landed first wins; ours is closed below, outside the lock,
        // since `close()` is async.
        let winner = {
            let mut map = self.unlocked.write().expect("unlocked map poisoned");
            match map.entry(id.to_string()) {
                Entry::Occupied(e) => Some(e.get().clone()),
                Entry::Vacant(e) => {
                    e.insert(pool.clone());
                    None
                }
            }
        };

        match winner {
            Some(winner) => {
                pool.close().await;
                Ok(winner)
            }
            None => {
                info!(user = %id, encrypted = user.is_encrypted(), "user database unlocked");
                Ok(pool)
            }
        }
    }

    /// Verifies the password and, for an encrypted user, recovers the DEK.
    /// `Ok(None)` means the user's database is not encrypted.
    async fn authenticate(
        &self,
        user: &User,
        password: Option<&str>,
    ) -> Result<Option<Dek>, AuthError> {
        match &user.credentials {
            Credentials::Encrypted { kdf_params, kdf_salt, database_password } => {
                let pw = password.ok_or(AuthError::PasswordRequired)?;
                let params = KdfParams::from_json(kdf_params)?;
                let kek = crypto::derive_kek(pw, kdf_salt, &params).await?;
                match crypto::unwrap_dek(&kek, database_password) {
                    Ok(dek) => Ok(Some(dek)),
                    Err(KeyError::WrongPassword) => Err(AuthError::WrongPassword),
                    Err(e) => Err(AuthError::Internal(anyhow!(e))),
                }
            }
            Credentials::Cleartext(Some(v)) => {
                let pw = password.ok_or(AuthError::PasswordRequired)?;
                let params = KdfParams::from_json(&v.kdf_params)?;
                let kek = crypto::derive_kek(pw, &v.kdf_salt, &params).await?;
                if !crypto::verify(&kek, &v.password_hash) {
                    return Err(AuthError::WrongPassword);
                }
                Ok(None)
            }
            // A role that has no login of its own. Who is allowed to ask for this
            // pool is the caller's problem — see the boundary note on the module.
            Credentials::Cleartext(None) => {
                if password.is_some() {
                    return Err(AuthError::PasswordNotSet);
                }
                Ok(None)
            }
        }
    }

    /// Drops the pool, which drops the key: the database is opaque again until
    /// the next login.
    pub async fn lock(&self, id: &str) {
        let pool = self.unlocked.write().expect("unlocked map poisoned").remove(id);
        if let Some(p) = pool {
            p.close().await;
            info!(user = %id, "user database locked");
        }
    }

    /// Shutdown: every key leaves RAM.
    pub async fn lock_all(&self) {
        let pools: Vec<_> = {
            let mut map = self.unlocked.write().expect("unlocked map poisoned");
            map.drain().collect()
        };
        for (id, pool) in pools {
            pool.close().await;
            info!(user = %id, "user database locked");
        }
    }

    // ── Lifecycle ─────────────────────────────────────────────────────────────

    /// Creates a user: opaque id, database file, then the directory row.
    ///
    /// The order is load-bearing. The row lives in `system.db` and the database
    /// in its own file, and no transaction spans the two. Writing the file first
    /// means a crash in between leaves an orphan file — harmless garbage — rather
    /// than a row whose database never existed, which would be a user that can
    /// authenticate and then find nothing to open.
    ///
    /// The new pool is closed before returning: creating a user is not logging in
    /// as them. The DEK is dropped here and survives only inside the seal.
    pub async fn register_user(
        &self,
        username: &str,
        display_name: Option<&str>,
        role_id: &str,
        password: Option<&str>,
        encrypted: bool,
    ) -> Result<String> {
        if username.trim().is_empty() {
            bail!("username must not be empty");
        }
        if db::users::by_username(&self.system, username).await?.is_some() {
            bail!("username already taken: {username}");
        }

        let id = uuid::Uuid::new_v4().to_string();
        let (credentials, dek) = self.mint_credentials(password, encrypted).await?;

        let path = self.path_of(&id);
        let pool = db::create_user_pool(&path, dek.as_ref())
            .await
            .with_context_path(&path)?;
        // The only moment this database is open with its key in hand before the
        // user ever logs in — so it is where the private memory store gets its
        // skeleton (`index.md` + `log.md`). Non-fatal: a user without a seeded
        // index is a worse assistant, not a broken account.
        if let Err(e) = crate::memory::scaffold::seed_private(&pool).await {
            warn!(user = %id, error = %e, "failed to seed private memory scaffold (non-fatal)");
        }
        pool.close().await;

        if let Err(e) =
            db::users::insert(&self.system, &id, username, display_name, role_id, &credentials).await
        {
            // The row never landed, so nothing points at this file. Take it back
            // out rather than leaving a database nobody can reach.
            self.remove_files(&path);
            return Err(e);
        }

        info!(user = %id, %username, encrypted, "user registered");
        Ok(id)
    }

    /// Builds the auth material for a new user, and the DEK when encrypted.
    async fn mint_credentials(
        &self,
        password: Option<&str>,
        encrypted: bool,
    ) -> Result<(Credentials, Option<Dek>)> {
        match (encrypted, password) {
            (true, None) => bail!("an encrypted user needs a password"),
            (true, Some(pw)) => {
                let salt = crypto::random_salt();
                let kek = crypto::derive_kek(pw, &salt, &self.kdf).await?;
                let dek = Dek::random();
                let sealed = crypto::wrap_dek(&kek, &dek)?;
                let creds = Credentials::Encrypted {
                    kdf_params: self.kdf.to_json()?,
                    kdf_salt: salt,
                    database_password: sealed,
                };
                Ok((creds, Some(dek)))
            }
            (false, Some(pw)) => {
                let salt = crypto::random_salt();
                let kek = crypto::derive_kek(pw, &salt, &self.kdf).await?;
                let creds = Credentials::Cleartext(Some(ClearVerifier {
                    kdf_params: self.kdf.to_json()?,
                    kdf_salt: salt,
                    password_hash: kek.as_verifier().to_vec(),
                }));
                Ok((creds, None))
            }
            (false, None) => Ok((Credentials::Cleartext(None), None)),
        }
    }

    /// Erases a user: the row first, then the file.
    ///
    /// The mirror of [`Self::register_user`], and for the same reason — a crash
    /// after the row is gone leaves an unreachable file, never a row without a
    /// database. GDPR erasure is then a matter of deleting files.
    pub async fn delete_user(&self, id: &str) -> Result<()> {
        self.lock(id).await;
        db::users::delete(&self.system, id).await?;
        self.remove_files(&self.path_of(id));
        info!(user = %id, "user deleted");
        Ok(())
    }

    /// Re-seals the same DEK under a key derived from the new password.
    ///
    /// The database is never re-encrypted and an unlocked pool stays valid: only
    /// the seal in `system.db` changes. Failing to open the old seal *is* the
    /// check that the old password was right.
    ///
    /// Migrating between encrypted and cleartext is **not** done here — that one
    /// needs `sqlcipher_export` to rewrite the file.
    pub async fn change_password(
        &self,
        id: &str,
        old: Option<&str>,
        new: Option<&str>,
    ) -> Result<(), AuthError> {
        let user = db::users::get(&self.system, id)
            .await
            .map_err(AuthError::Internal)?
            .ok_or(AuthError::UnknownUser)?;

        let credentials = match &user.credentials {
            Credentials::Encrypted { kdf_params, kdf_salt, database_password } => {
                let old = old.ok_or(AuthError::PasswordRequired)?;
                let new = new.ok_or_else(|| {
                    AuthError::Internal(anyhow!("an encrypted user cannot drop its password"))
                })?;

                let params = KdfParams::from_json(kdf_params)?;
                let kek_old = crypto::derive_kek(old, kdf_salt, &params).await?;
                let dek = match crypto::unwrap_dek(&kek_old, database_password) {
                    Ok(dek) => dek,
                    Err(KeyError::WrongPassword) => return Err(AuthError::WrongPassword),
                    Err(e) => return Err(AuthError::Internal(anyhow!(e))),
                };

                let salt = crypto::random_salt();
                let kek_new = crypto::derive_kek(new, &salt, &self.kdf).await?;
                Credentials::Encrypted {
                    kdf_params: self.kdf.to_json()?,
                    kdf_salt: salt,
                    database_password: crypto::wrap_dek(&kek_new, &dek)?,
                }
            }

            Credentials::Cleartext(existing) => {
                match (existing, old) {
                    (Some(v), Some(old)) => {
                        let params = KdfParams::from_json(&v.kdf_params)?;
                        let kek = crypto::derive_kek(old, &v.kdf_salt, &params).await?;
                        if !crypto::verify(&kek, &v.password_hash) {
                            return Err(AuthError::WrongPassword);
                        }
                    }
                    (Some(_), None) => return Err(AuthError::PasswordRequired),
                    (None, Some(_)) => return Err(AuthError::PasswordNotSet),
                    (None, None) => {}
                }
                match new {
                    None => Credentials::Cleartext(None),
                    Some(new) => {
                        let salt = crypto::random_salt();
                        let kek = crypto::derive_kek(new, &salt, &self.kdf).await?;
                        Credentials::Cleartext(Some(ClearVerifier {
                            kdf_params: self.kdf.to_json()?,
                            kdf_salt: salt,
                            password_hash: kek.as_verifier().to_vec(),
                        }))
                    }
                }
            }
        };

        db::users::set_credentials(&self.system, id, &credentials)
            .await
            .map_err(AuthError::Internal)?;
        info!(user = %id, "password changed");
        Ok(())
    }

    /// Best-effort: a leftover file is garbage, not a correctness problem, and a
    /// failure to remove it must not fail the operation that asked.
    fn remove_files(&self, path: &Path) {
        for p in std::iter::once(path.to_path_buf()).chain(db::user_db_sidecars(path)) {
            if p.exists()
                && let Err(e) = std::fs::remove_file(&p)
            {
                warn!(path = %p.display(), error = %e, "could not remove database file");
            }
        }
    }
}

/// Adds the file path to a provisioning failure, which otherwise surfaces as a
/// bare sqlx error with no hint of which database it was.
trait ContextPath<T> {
    fn with_context_path(self, path: &Path) -> Result<T>;
}

impl<T> ContextPath<T> for Result<T> {
    fn with_context_path(self, path: &Path) -> Result<T> {
        self.map_err(|e| e.context(format!("provisioning {}", path.display())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture {
        dir: PathBuf,
        users: UserManager,
    }

    impl Fixture {
        async fn new(tag: &str) -> Self {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
            let mut dir = std::env::temp_dir();
            dir.push(format!("skald-um-{tag}-{}-{nanos}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();

            let system = db::init_system_pool(dir.join("system.db").to_str().unwrap())
                .await
                .unwrap();
            // Seed the non-admin role some tests use, so the FK on
            // users.role_id → roles(id) holds.
            db::roles::insert(&system, "children", "Children", "default", None)
                .await
                .unwrap();

            let users = UserManager {
                system: Arc::new(system),
                unlocked: RwLock::new(HashMap::new()),
                db_dir: dir.clone(),
                // The real 256 MiB / ~1s derivation, times two per test, is not
                // something a test suite should pay.
                kdf: KdfParams::fast(),
            };
            Fixture { dir, users }
        }

        fn path_of(&self, id: &str) -> PathBuf {
            self.users.path_of(id)
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    async fn write_marker(pool: &SqlitePool, title: &str) {
        sqlx::query("INSERT INTO chat_sessions (title) VALUES (?1)")
            .bind(title)
            .execute(pool)
            .await
            .unwrap();
    }

    async fn read_marker(pool: &SqlitePool) -> String {
        sqlx::query_scalar::<_, String>("SELECT title FROM chat_sessions LIMIT 1")
            .fetch_one(pool)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn register_writes_the_file_before_the_row() {
        let f = Fixture::new("order").await;
        let id = f.users.register_user("ada", Some("Ada"), "admin", Some("pw"), true).await.unwrap();

        assert!(f.path_of(&id).exists(), "the database file must exist");
        assert_eq!(f.users.count().await.unwrap(), 1);
        // Creating a user is not logging in as them.
        assert!(!f.users.is_unlocked(&id), "register must not leave the pool unlocked");
    }

    #[tokio::test]
    async fn a_duplicate_username_leaves_no_orphan_file() {
        let f = Fixture::new("dup").await;
        f.users.register_user("ada", None, "admin", Some("pw"), true).await.unwrap();

        let before: Vec<_> = std::fs::read_dir(&f.dir).unwrap().collect();
        assert!(f.users.register_user("ada", None, "admin", Some("pw2"), true).await.is_err());
        let after: Vec<_> = std::fs::read_dir(&f.dir).unwrap().collect();

        assert_eq!(before.len(), after.len(), "the failed registration must clean up its file");
        assert_eq!(f.users.count().await.unwrap(), 1);
    }

    /// The full §9 cycle: unlock at login, key in RAM until the pool is dropped.
    #[tokio::test]
    async fn encrypted_user_round_trips_through_lock_and_unlock() {
        let f = Fixture::new("cycle").await;
        let id = f.users.register_user("ada", None, "admin", Some("pw"), true).await.unwrap();

        let pool = f.users.open_db(&id, Some("pw")).await.unwrap();
        write_marker(&pool, "private").await;
        assert!(f.users.is_unlocked(&id));

        f.users.lock(&id).await;
        assert!(!f.users.is_unlocked(&id));
        assert!(f.users.pool_of(&id).is_none(), "locked means no pool");

        let pool = f.users.open_db(&id, Some("pw")).await.unwrap();
        assert_eq!(read_marker(&pool).await, "private");

        // The file on disk is genuinely encrypted, not merely gated by our code.
        let raw = std::fs::read(f.path_of(&id)).unwrap();
        assert_ne!(&raw[..16], b"SQLite format 3\0");
        assert!(!String::from_utf8_lossy(&raw).contains("private"));
    }

    #[tokio::test]
    async fn a_wrong_password_is_rejected_and_unlocks_nothing() {
        let f = Fixture::new("wrongpw").await;
        let id = f.users.register_user("ada", None, "admin", Some("pw"), true).await.unwrap();

        let err = f.users.open_db(&id, Some("nope")).await.unwrap_err();
        assert!(matches!(err, AuthError::WrongPassword), "got {err:?}");
        assert!(!f.users.is_unlocked(&id));

        assert!(matches!(f.users.open_db(&id, None).await.unwrap_err(), AuthError::PasswordRequired));
        assert!(matches!(f.users.open_db("ghost", Some("pw")).await.unwrap_err(), AuthError::UnknownUser));
    }

    /// A row without a file is the one state the ordering rule forbids. If it
    /// happens anyway, say so — never silently hand back an empty database.
    #[tokio::test]
    async fn a_missing_file_is_reported_not_recreated() {
        let f = Fixture::new("missing").await;
        let id = f.users.register_user("ada", None, "admin", Some("pw"), true).await.unwrap();
        std::fs::remove_file(f.path_of(&id)).unwrap();

        let err = f.users.open_db(&id, Some("pw")).await.unwrap_err();
        assert!(matches!(err, AuthError::MissingDatabase(_)), "got {err:?}");
        assert!(!f.path_of(&id).exists(), "a failed login must not create a database");
    }

    #[tokio::test]
    async fn changing_the_password_reseals_the_same_key() {
        let f = Fixture::new("rewrap").await;
        let id = f.users.register_user("ada", None, "admin", Some("old"), true).await.unwrap();

        let pool = f.users.open_db(&id, Some("old")).await.unwrap();
        write_marker(&pool, "kept").await;

        f.users.change_password(&id, Some("old"), Some("new")).await.unwrap();
        // The DEK never changed, so the pool we are holding is still live.
        assert_eq!(read_marker(&pool).await, "kept");

        f.users.lock(&id).await;
        assert!(matches!(
            f.users.change_password(&id, Some("old"), Some("x")).await.unwrap_err(),
            AuthError::WrongPassword
        ));

        // ...and the data is still there, reachable only with the new password.
        assert!(f.users.open_db(&id, Some("old")).await.is_err());
        let pool = f.users.open_db(&id, Some("new")).await.unwrap();
        assert_eq!(read_marker(&pool).await, "kept");
    }

    #[tokio::test]
    async fn cleartext_users_authenticate_without_encrypting_anything() {
        let f = Fixture::new("clear").await;
        let pinned = f.users.register_user("kid", None, "children", Some("pin"), false).await.unwrap();
        let open = f.users.register_user("kiosk", None, "children", None, false).await.unwrap();

        // A verifier is checked; the database itself is a plain SQLite file, which
        // is what makes supervision possible at all (§12).
        assert!(matches!(f.users.open_db(&pinned, Some("no")).await.unwrap_err(), AuthError::WrongPassword));
        let pool = f.users.open_db(&pinned, Some("pin")).await.unwrap();
        write_marker(&pool, "homework").await;
        assert_eq!(&std::fs::read(f.path_of(&pinned)).unwrap()[..16], b"SQLite format 3\0");

        // No verifier: no password to give, and offering one is a caller bug.
        assert!(matches!(f.users.open_db(&open, Some("x")).await.unwrap_err(), AuthError::PasswordNotSet));
        f.users.open_db(&open, None).await.unwrap();
    }

    #[tokio::test]
    async fn open_db_is_idempotent_and_returns_the_same_pool() {
        let f = Fixture::new("idem").await;
        let id = f.users.register_user("ada", None, "admin", Some("pw"), true).await.unwrap();

        let a = f.users.open_db(&id, Some("pw")).await.unwrap();
        // No password needed the second time: the pool *is* the unlock token.
        let b = f.users.open_db(&id, None).await.unwrap();
        write_marker(&a, "shared").await;
        assert_eq!(read_marker(&b).await, "shared");
    }

    #[tokio::test]
    async fn deleting_a_user_evicts_the_pool_and_erases_the_files() {
        let f = Fixture::new("erase").await;
        let id = f.users.register_user("ada", None, "admin", Some("pw"), true).await.unwrap();
        let pool = f.users.open_db(&id, Some("pw")).await.unwrap();
        write_marker(&pool, "gone").await;

        f.users.delete_user(&id).await.unwrap();

        assert!(!f.users.is_unlocked(&id));
        assert_eq!(f.users.count().await.unwrap(), 0);
        assert!(!f.path_of(&id).exists());
        for sidecar in db::user_db_sidecars(&f.path_of(&id)) {
            assert!(!sidecar.exists(), "{} survived erasure", sidecar.display());
        }
    }

    #[tokio::test]
    async fn lock_all_drops_every_key() {
        let f = Fixture::new("lockall").await;
        let a = f.users.register_user("ada", None, "admin", Some("pw"), true).await.unwrap();
        let b = f.users.register_user("bob", None, "admin", None, false).await.unwrap();
        f.users.open_db(&a, Some("pw")).await.unwrap();
        f.users.open_db(&b, None).await.unwrap();

        f.users.lock_all().await;
        assert!(!f.users.is_unlocked(&a) && !f.users.is_unlocked(&b));
    }

    #[tokio::test]
    async fn auth_errors_never_carry_key_material() {
        let f = Fixture::new("noleak").await;
        let id = f.users.register_user("ada", None, "admin", Some("hunter2"), true).await.unwrap();
        let err = f.users.open_db(&id, Some("hunter2!")).await.unwrap_err();
        let printed = format!("{err:?} {err}");
        assert!(!printed.contains("hunter2"), "the password must not reach an error");
    }
}
