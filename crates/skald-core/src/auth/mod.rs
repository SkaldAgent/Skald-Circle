//! Session management — the layer above `UserManager`.
//!
//! `SessionStore` maps an opaque session token to a user id, in RAM only.
//! It knows nothing about HTTP cookies: the handler layer parses the cookie
//! header and calls [`SessionStore::user_of`] / [`SessionStore::logout`].
//!
//! The session lifetime mirrors the process lifetime: on restart every pool is
//! dropped (§9), so a surviving session would be useless — the user must
//! re-authenticate to unlock the database anyway.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use tracing::info;

use crate::users::{AuthError, UserManager};

/// Cookie name used across all auth endpoints.
pub const COOKIE_NAME: &str = "skald_session";

pub struct SessionStore {
    users:    Arc<UserManager>,
    sessions: RwLock<HashMap<String, String>>,
}

impl SessionStore {
    pub fn new(users: Arc<UserManager>) -> Self {
        Self {
            users,
            sessions: RwLock::new(HashMap::new()),
        }
    }

    /// Authenticates the user and unlocks their database, then mints a session
    /// token. Returns the token so the caller can set it as a cookie.
    pub async fn login(
        &self,
        username: &str,
        password: &str,
    ) -> Result<String, AuthError> {
        let user = self
            .users
            .by_username(username)
            .await
            .map_err(AuthError::Internal)?
            .ok_or(AuthError::UnknownUser)?;

        // Always verify the password, even if the pool is already unlocked —
        // open_db's idempotency short-circuit would skip the check otherwise.
        self.users.verify_credentials(&user.id, password).await?;

        // open_db handles pool lifecycle: unlocks if needed, no-op if open.
        self.users.open_db(&user.id, Some(password)).await?;

        let token = uuid::Uuid::new_v4().to_string();
        self.sessions
            .write()
            .expect("sessions map poisoned")
            .insert(token.clone(), user.id.clone());

        info!(user = %user.id, %username, "session created");
        Ok(token)
    }

    /// Returns the user id for a session token, or `None` if unknown / expired.
    pub fn user_of(&self, token: &str) -> Option<String> {
        self.sessions
            .read()
            .ok()?
            .get(token)
            .cloned()
    }

    /// Removes **every** session of one user, returning how many were dropped.
    ///
    /// The admin half of [`logout`](Self::logout): a deactivated or deleted user
    /// must stop being authenticated *now*, not at their next request. `login`
    /// already refuses an inactive user (`verify_credentials` / `open_db` check the
    /// flag), but `require_auth` only maps token → id and would happily keep serving
    /// a token minted before the flag flipped.
    ///
    /// Sessions only, deliberately: this leaves the pool open, so the caller must
    /// follow with `UserManager::lock` to get the key out of RAM (§9). Both run
    /// synchronously on the admin's request — revocation is an invariant, not
    /// something to reconcile later on a lossy bus.
    pub fn revoke_user(&self, user_id: &str) -> usize {
        let mut map = self.sessions.write().expect("sessions map poisoned");
        let before = map.len();
        map.retain(|_, id| id != user_id);
        let removed = before - map.len();
        if removed > 0 {
            info!(user = %user_id, sessions = removed, "sessions revoked");
        }
        removed
    }

    /// Records a session without authenticating — tests only, so the revocation
    /// semantics can be exercised without paying an Argon2id derivation per login.
    #[cfg(test)]
    fn insert_session(&self, token: &str, user_id: &str) {
        self.sessions
            .write()
            .expect("sessions map poisoned")
            .insert(token.to_string(), user_id.to_string());
    }

    /// Removes a single session. The database pool stays open (§9).
    pub fn logout(&self, token: &str) {
        if let Some(user_id) = self
            .sessions
            .write()
            .expect("sessions map poisoned")
            .remove(token)
        {
            info!(user = %user_id, "session removed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_db_path(tag: &str) -> String {
        let mut p = std::env::temp_dir();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        p.push(format!("skald-test-{tag}-{}-{nanos}.db", std::process::id()));
        p.to_string_lossy().into_owned()
    }

    fn cleanup(path: &str) {
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{path}{suffix}"));
        }
    }

    async fn store(tag: &str) -> (SessionStore, String) {
        let path = temp_db_path(tag);
        let pool = Arc::new(crate::db::init_system_pool(&path).await.unwrap());
        (SessionStore::new(Arc::new(UserManager::new(pool))), path)
    }

    /// Deactivating or deleting a user must drop **every** session they hold — one
    /// browser left logged in is the whole bug — and **only** theirs.
    #[tokio::test]
    async fn revoke_user_drops_all_of_one_users_sessions_and_nobody_elses() {
        let (store, path) = store("revoke").await;
        store.insert_session("t-laptop", "u-1");
        store.insert_session("t-phone",  "u-1");
        store.insert_session("t-other",  "u-2");

        assert_eq!(store.revoke_user("u-1"), 2);
        assert_eq!(store.user_of("t-laptop"), None);
        assert_eq!(store.user_of("t-phone"),  None);
        assert_eq!(store.user_of("t-other").as_deref(), Some("u-2"),
            "revoking one user must not log out the rest of the household");

        cleanup(&path);
    }

    /// Idempotent: revoking a user with nothing live is a no-op, not an error — the
    /// admin path calls it unconditionally.
    #[tokio::test]
    async fn revoke_user_is_a_no_op_when_nothing_is_live() {
        let (store, path) = store("revoke-empty").await;
        store.insert_session("t-other", "u-2");

        assert_eq!(store.revoke_user("u-1"), 0);
        assert_eq!(store.user_of("t-other").as_deref(), Some("u-2"));

        cleanup(&path);
    }
}
