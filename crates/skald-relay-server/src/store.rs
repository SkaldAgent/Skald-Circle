//! SQLite persistence (relay.md §3). No sensitive data in the clear: the
//! `ciphertext` blobs are E2E, the pubkeys are public identifiers. The API is
//! designed to be swappable for Postgres+Redis post-v1 (relay.md §3 "scale
//! path"); for now there is a single writer (the SQLite-on-EFS constraint).

use std::collections::HashSet;
use std::str::FromStr;
use std::time::Duration;

use anyhow::Result;
use sqlx::Row;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions};

use crate::auth::ct_eq;

/// Current unix milliseconds (application timestamp encoding, index.md §5).
pub fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

fn to_arr<const N: usize>(v: &[u8]) -> Option<[u8; N]> {
    if v.len() != N {
        return None;
    }
    let mut out = [0u8; N];
    out.copy_from_slice(v);
    Some(out)
}

/// Persisted client state.
#[derive(Debug, Clone)]
pub struct ClientRow {
    pub x25519_pub: [u8; 32],
    pub device_token: Option<String>,
    pub platform: String,
    pub state: String, // 'pending' | 'authorized'
}

/// A store-and-forward queued message.
#[derive(Debug, Clone)]
pub struct QueuedMsg {
    pub id: i64,
    pub from_pub: [u8; 32],
    pub nonce: [u8; 12],
    pub ciphertext: Vec<u8>,
    pub created_at: i64,
}

/// A client in `pending` state (for re-sending `client_paired` to the agent).
#[derive(Debug, Clone)]
pub struct PendingClient {
    pub ed25519_pub: [u8; 32],
    pub x25519_pub: [u8; 32],
    pub platform: String,
}

#[derive(Clone)]
pub struct Store {
    pool: SqlitePool,
}

impl Store {
    /// Open/create the DB and apply the schema (idempotent). No WAL: the deploy
    /// EFS/NFS does not support it; `busy_timeout` serializes the single writer.
    pub async fn init(path: &str) -> Result<Store> {
        let opts = SqliteConnectOptions::from_str(&format!("sqlite://{path}"))?
            .create_if_missing(true)
            .busy_timeout(Duration::from_secs(5))
            .foreign_keys(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(4)
            .connect_with(opts)
            .await?;

        for stmt in SCHEMA {
            // SCHEMA entries are 'static string literals (audited, no user data).
            sqlx::query(*stmt).execute(&pool).await?;
        }
        Ok(Store { pool })
    }

    // ----- namespaces ---------------------------------------------------------

    /// Create the namespace if absent (binding it immutably to the pubkey) and
    /// bump `last_active`. Idempotent.
    pub async fn upsert_namespace(&self, ns: &str, agent_pub: &[u8; 32]) -> Result<()> {
        let now = now_ms();
        sqlx::query(
            "INSERT INTO namespaces (namespace_id, agent_ed25519_pub, created_at, last_active)
             VALUES (?1, ?2, ?3, ?3)
             ON CONFLICT(namespace_id) DO UPDATE SET last_active = ?3",
        )
        .bind(ns)
        .bind(&agent_pub[..])
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// `true` if the namespace exists.
    pub async fn namespace_exists(&self, ns: &str) -> Result<bool> {
        let row = sqlx::query("SELECT 1 FROM namespaces WHERE namespace_id = ?1")
            .bind(ns)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.is_some())
    }

    /// The namespace agent's ed25519 pubkey (None if it does not exist).
    pub async fn agent_pub(&self, ns: &str) -> Result<Option<[u8; 32]>> {
        let row = sqlx::query("SELECT agent_ed25519_pub FROM namespaces WHERE namespace_id = ?1")
            .bind(ns)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.and_then(|r| {
            let b: Vec<u8> = r.get(0);
            to_arr::<32>(&b)
        }))
    }

    pub async fn touch_namespace(&self, ns: &str) -> Result<()> {
        sqlx::query("UPDATE namespaces SET last_active = ?2 WHERE namespace_id = ?1")
            .bind(ns)
            .bind(now_ms())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    // ----- pairing ------------------------------------------------------------

    /// Open/replace the pairing window. `expiry_ms` already computed.
    pub async fn pairing_start(&self, ns: &str, token: &[u8; 32], expiry_ms: i64) -> Result<()> {
        sqlx::query(
            "UPDATE namespaces
             SET pairing_token = ?2, pairing_expiry = ?3, pairing_consumed = 0
             WHERE namespace_id = ?1",
        )
        .bind(ns)
        .bind(&token[..])
        .bind(expiry_ms)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn pairing_stop(&self, ns: &str) -> Result<()> {
        sqlx::query(
            "UPDATE namespaces
             SET pairing_token = NULL, pairing_expiry = NULL, pairing_consumed = 0
             WHERE namespace_id = ?1",
        )
        .bind(ns)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Try to consume the pairing token (single-use, constant-time, not
    /// expired). Returns `true` if pairing is allowed to proceed.
    pub async fn consume_pairing_token(&self, ns: &str, token: &[u8; 32]) -> Result<bool> {
        let row = sqlx::query(
            "SELECT pairing_token, pairing_expiry, pairing_consumed
             FROM namespaces WHERE namespace_id = ?1",
        )
        .bind(ns)
        .fetch_optional(&self.pool)
        .await?;

        let Some(row) = row else { return Ok(false) };
        let stored: Option<Vec<u8>> = row.get(0);
        let expiry: Option<i64> = row.get(1);
        let consumed: i64 = row.get(2);

        let (Some(stored), Some(expiry)) = (stored, expiry) else {
            return Ok(false); // no open window
        };
        if consumed != 0 || expiry <= now_ms() || !ct_eq(&stored, &token[..]) {
            return Ok(false);
        }

        // Atomic guard against concurrent double-consume.
        let res = sqlx::query(
            "UPDATE namespaces SET pairing_consumed = 1
             WHERE namespace_id = ?1 AND pairing_consumed = 0",
        )
        .bind(ns)
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected() == 1)
    }

    // ----- clients ------------------------------------------------------------

    /// Register/update a client as `pending` (after a successful pairing).
    pub async fn upsert_pending_client(
        &self,
        ns: &str,
        ed_pub: &[u8; 32],
        x_pub: &[u8; 32],
        device_token: &str,
        platform: &str,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO clients
               (namespace_id, client_ed25519_pub, client_x25519_pub, device_token, platform, state, last_seen)
             VALUES (?1, ?2, ?3, ?4, ?5, 'pending', ?6)
             ON CONFLICT(namespace_id, client_ed25519_pub) DO UPDATE SET
               client_x25519_pub = ?3,
               device_token = CASE WHEN ?4 = '' THEN clients.device_token ELSE ?4 END,
               platform = ?5, state = 'pending', last_seen = ?6",
        )
        .bind(ns)
        .bind(&ed_pub[..])
        .bind(&x_pub[..])
        .bind(device_token)
        .bind(platform)
        .bind(now_ms())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn get_client(&self, ns: &str, ed_pub: &[u8; 32]) -> Result<Option<ClientRow>> {
        let row = sqlx::query(
            "SELECT client_x25519_pub, device_token, platform, state
             FROM clients WHERE namespace_id = ?1 AND client_ed25519_pub = ?2",
        )
        .bind(ns)
        .bind(&ed_pub[..])
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.and_then(|r| {
            let x: Vec<u8> = r.get(0);
            Some(ClientRow {
                x25519_pub: to_arr::<32>(&x)?,
                device_token: r.get::<Option<String>, _>(1),
                platform: r.get(2),
                state: r.get(3),
            })
        }))
    }

    pub async fn is_authorized_client(&self, ns: &str, ed_pub: &[u8; 32]) -> Result<bool> {
        let row = sqlx::query(
            "SELECT 1 FROM clients
             WHERE namespace_id = ?1 AND client_ed25519_pub = ?2 AND state = 'authorized'",
        )
        .bind(ns)
        .bind(&ed_pub[..])
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.is_some())
    }

    /// Update the client's push token (APNs/FCM rotate it) + last_seen.
    ///
    /// An **empty** `device_token` is treated as "no token available right now"
    /// (e.g. the device connected before its APNs registration completed) and
    /// must NOT clobber a previously stored, valid token — otherwise every push
    /// to that client fails with `MissingDeviceToken`. In that case we still
    /// bump `last_seen` but keep the existing token.
    pub async fn update_client_device_token(
        &self,
        ns: &str,
        ed_pub: &[u8; 32],
        device_token: &str,
    ) -> Result<()> {
        sqlx::query(
            "UPDATE clients
                SET device_token = CASE WHEN ?3 = '' THEN device_token ELSE ?3 END,
                    last_seen = ?4
             WHERE namespace_id = ?1 AND client_ed25519_pub = ?2",
        )
        .bind(ns)
        .bind(&ed_pub[..])
        .bind(device_token)
        .bind(now_ms())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn list_pending_clients(&self, ns: &str) -> Result<Vec<PendingClient>> {
        let rows = sqlx::query(
            "SELECT client_ed25519_pub, client_x25519_pub, platform
             FROM clients WHERE namespace_id = ?1 AND state = 'pending'",
        )
        .bind(ns)
        .fetch_all(&self.pool)
        .await?;
        let mut out = Vec::new();
        for r in rows {
            let ed: Vec<u8> = r.get(0);
            let x: Vec<u8> = r.get(1);
            if let (Some(ed), Some(x)) = (to_arr::<32>(&ed), to_arr::<32>(&x)) {
                out.push(PendingClient {
                    ed25519_pub: ed,
                    x25519_pub: x,
                    platform: r.get(2),
                });
            }
        }
        Ok(out)
    }

    /// Apply `authorize` (replace semantics, relay-protocol.md §6). Returns
    /// `(authorized count, revoked pubkeys)`. Revoked clients must then be
    /// disconnected; their queue has already been purged here.
    pub async fn apply_authorize(
        &self,
        ns: &str,
        new_list: &[[u8; 32]],
    ) -> Result<(i64, Vec<[u8; 32]>)> {
        let new_set: HashSet<Vec<u8>> = new_list.iter().map(|k| k.to_vec()).collect();

        let existing =
            sqlx::query("SELECT client_ed25519_pub FROM clients WHERE namespace_id = ?1")
                .bind(ns)
                .fetch_all(&self.pool)
                .await?;

        let mut revoked = Vec::new();
        for r in existing {
            let pub_bytes: Vec<u8> = r.get(0);
            if new_set.contains(&pub_bytes) {
                // Present in the new list → authorized (leaves pending).
                sqlx::query(
                    "UPDATE clients SET state = 'authorized'
                     WHERE namespace_id = ?1 AND client_ed25519_pub = ?2",
                )
                .bind(ns)
                .bind(&pub_bytes)
                .execute(&self.pool)
                .await?;
            } else {
                // Absent → revoked: purge queue, forget device_token, remove.
                self.purge_queue_for_bytes(ns, &pub_bytes).await?;
                sqlx::query(
                    "DELETE FROM clients WHERE namespace_id = ?1 AND client_ed25519_pub = ?2",
                )
                .bind(ns)
                .bind(&pub_bytes)
                .execute(&self.pool)
                .await?;
                if let Some(k) = to_arr::<32>(&pub_bytes) {
                    revoked.push(k);
                }
            }
        }

        let count: i64 = sqlx::query(
            "SELECT COUNT(*) FROM clients WHERE namespace_id = ?1 AND state = 'authorized'",
        )
        .bind(ns)
        .fetch_one(&self.pool)
        .await?
        .get(0);

        Ok((count, revoked))
    }

    // ----- queue (store-and-forward) -----------------------------------------

    pub async fn queue_count(&self, ns: &str, to_pub: &[u8; 32]) -> Result<i64> {
        let n: i64 =
            sqlx::query("SELECT COUNT(*) FROM queue WHERE namespace_id = ?1 AND to_pub = ?2")
                .bind(ns)
                .bind(&to_pub[..])
                .fetch_one(&self.pool)
                .await?
                .get(0);
        Ok(n)
    }

    /// Enqueue a message. `Ok(false)` if the recipient's queue is full.
    pub async fn enqueue(
        &self,
        ns: &str,
        to_pub: &[u8; 32],
        from_pub: &[u8; 32],
        nonce: &[u8; 12],
        ciphertext: &[u8],
        max_per_dest: i64,
    ) -> Result<bool> {
        if self.queue_count(ns, to_pub).await? >= max_per_dest {
            return Ok(false);
        }
        sqlx::query(
            "INSERT INTO queue (namespace_id, to_pub, from_pub, nonce, ciphertext, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )
        .bind(ns)
        .bind(&to_pub[..])
        .bind(&from_pub[..])
        .bind(&nonce[..])
        .bind(ciphertext)
        .bind(now_ms())
        .execute(&self.pool)
        .await?;
        Ok(true)
    }

    pub async fn fetch_pending(&self, ns: &str, to_pub: &[u8; 32]) -> Result<Vec<QueuedMsg>> {
        let rows = sqlx::query(
            "SELECT id, from_pub, nonce, ciphertext, created_at
             FROM queue WHERE namespace_id = ?1 AND to_pub = ?2 ORDER BY id ASC",
        )
        .bind(ns)
        .bind(&to_pub[..])
        .fetch_all(&self.pool)
        .await?;
        let mut out = Vec::new();
        for r in rows {
            let from: Vec<u8> = r.get(1);
            let nonce: Vec<u8> = r.get(2);
            if let (Some(from), Some(nonce)) = (to_arr::<32>(&from), to_arr::<12>(&nonce)) {
                out.push(QueuedMsg {
                    id: r.get(0),
                    from_pub: from,
                    nonce,
                    ciphertext: r.get(3),
                    created_at: r.get(4),
                });
            }
        }
        Ok(out)
    }

    pub async fn delete_pending(&self, id: i64) -> Result<()> {
        sqlx::query("DELETE FROM queue WHERE id = ?1")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn purge_queue_for_bytes(&self, ns: &str, to_pub: &[u8]) -> Result<()> {
        sqlx::query("DELETE FROM queue WHERE namespace_id = ?1 AND to_pub = ?2")
            .bind(ns)
            .bind(to_pub)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    // ----- garbage collection -------------------------------------------------

    /// Delete messages older than `ttl_days` and namespaces idle for `ttl_days`
    /// (cascade to clients + queue). Returns `(messages, namespaces)` removed.
    pub async fn gc(&self, ttl_days: i64) -> Result<(u64, u64)> {
        let cutoff = now_ms() - ttl_days * 24 * 60 * 60 * 1000;
        let msgs = sqlx::query("DELETE FROM queue WHERE created_at < ?1")
            .bind(cutoff)
            .execute(&self.pool)
            .await?
            .rows_affected();
        let namespaces = sqlx::query("DELETE FROM namespaces WHERE last_active < ?1")
            .bind(cutoff)
            .execute(&self.pool)
            .await?
            .rows_affected();
        Ok((msgs, namespaces))
    }
}

const SCHEMA: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS namespaces (
        namespace_id      TEXT PRIMARY KEY,
        agent_ed25519_pub BLOB NOT NULL UNIQUE,
        created_at        INTEGER NOT NULL,
        last_active       INTEGER NOT NULL,
        pairing_token     BLOB,
        pairing_expiry    INTEGER,
        pairing_consumed  INTEGER NOT NULL DEFAULT 0
    )",
    "CREATE TABLE IF NOT EXISTS clients (
        namespace_id       TEXT NOT NULL REFERENCES namespaces(namespace_id) ON DELETE CASCADE,
        client_ed25519_pub BLOB NOT NULL,
        client_x25519_pub  BLOB NOT NULL,
        device_token       TEXT,
        platform           TEXT NOT NULL,
        state              TEXT NOT NULL,
        last_seen          INTEGER,
        PRIMARY KEY (namespace_id, client_ed25519_pub)
    )",
    "CREATE TABLE IF NOT EXISTS queue (
        id            INTEGER PRIMARY KEY AUTOINCREMENT,
        namespace_id  TEXT NOT NULL REFERENCES namespaces(namespace_id) ON DELETE CASCADE,
        to_pub        BLOB NOT NULL,
        from_pub      BLOB NOT NULL,
        nonce         BLOB NOT NULL,
        ciphertext    BLOB NOT NULL,
        created_at    INTEGER NOT NULL
    )",
    "CREATE INDEX IF NOT EXISTS idx_queue_dest ON queue(namespace_id, to_pub, id)",
];

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    async fn temp_store() -> Store {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "relay-store-ut-{nanos}-{}-{seq}.db",
            std::process::id()
        ));
        Store::init(&path.to_string_lossy()).await.expect("init store")
    }

    /// An empty `device_token` (device connected before APNs registration
    /// finished) must NOT wipe a previously stored, valid token — otherwise
    /// every push fails with `MissingDeviceToken`.
    #[tokio::test]
    async fn empty_device_token_does_not_clobber() {
        let s = temp_store().await;
        let ns = "a".repeat(64);
        let ed = [1u8; 32];
        let x = [2u8; 32];
        s.upsert_namespace(&ns, &[9u8; 32]).await.unwrap();

        // Pair with a real token.
        s.upsert_pending_client(&ns, &ed, &x, "realtoken", "ios").await.unwrap();
        assert_eq!(
            s.get_client(&ns, &ed).await.unwrap().unwrap().device_token.as_deref(),
            Some("realtoken")
        );

        // A later connect with an empty token keeps the existing one.
        s.update_client_device_token(&ns, &ed, "").await.unwrap();
        assert_eq!(
            s.get_client(&ns, &ed).await.unwrap().unwrap().device_token.as_deref(),
            Some("realtoken")
        );

        // A non-empty token still updates.
        s.update_client_device_token(&ns, &ed, "rotated").await.unwrap();
        assert_eq!(
            s.get_client(&ns, &ed).await.unwrap().unwrap().device_token.as_deref(),
            Some("rotated")
        );

        // Re-pairing with an empty token also preserves the stored one.
        s.upsert_pending_client(&ns, &ed, &x, "", "ios").await.unwrap();
        assert_eq!(
            s.get_client(&ns, &ed).await.unwrap().unwrap().device_token.as_deref(),
            Some("rotated")
        );
    }
}
