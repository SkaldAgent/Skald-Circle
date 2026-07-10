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
