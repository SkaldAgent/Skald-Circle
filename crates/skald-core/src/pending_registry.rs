//! Generic in-memory registry for pending human-in-the-loop requests.
//!
//! Approval, clarification, and elicitation all share the same shape: a request
//! is registered under an id with some display `Info`, a caller blocks on a
//! `oneshot::Receiver<Resolution>`, and later something resolves the request by
//! id — firing the sender and dropping the entry. This type factors out that
//! shared plumbing (the `Mutex<HashMap>` + oneshot bookkeeping) so each manager
//! keeps only what is genuinely its own: id minting, event emission, and any
//! extra policy (rules/bypass for approval, secret handling for elicitation).
//!
//! What deliberately stays OUT of the registry:
//! - **id minting** — the caller supplies the key (a durable `tool_call_id` for
//!   approval, an internal counter for clarification/elicitation);
//! - **event emission** — the `ServerEvent` variants differ per manager, so the
//!   caller broadcasts after `insert` / `resolve`;
//! - **ordering** — `list()` is unsorted; callers that need a stable order sort
//!   on their own `Info` field (e.g. `created_at`).

use std::collections::HashMap;

use tokio::sync::{Mutex, oneshot};

/// One registered request: its display `Info` and the sender that unblocks the
/// waiting caller with a `Resolution`.
struct Entry<I, R> {
    info: I,
    tx:   oneshot::Sender<R>,
}

/// Keyed store of pending requests. `I` is the cloneable public info surfaced to
/// the Inbox; `R` is the resolution payload delivered back to the blocked caller.
pub struct PendingRegistry<I, R> {
    pending: Mutex<HashMap<i64, Entry<I, R>>>,
}

impl<I: Clone, R> PendingRegistry<I, R> {
    pub fn new() -> Self {
        Self { pending: Mutex::new(HashMap::new()) }
    }

    /// Registers `info` under `id` and returns the receiver the caller awaits.
    /// The caller mints `id` (a durable tool_call_id or an internal counter).
    pub async fn insert(&self, id: i64, info: I) -> oneshot::Receiver<R> {
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, Entry { info, tx });
        rx
    }

    /// Removes the entry for `id` and delivers `resolution` to the waiting caller.
    /// Returns the entry's `info` (so the caller can broadcast a resolved event),
    /// or `None` when no live entry exists (already resolved, or post-restart).
    pub async fn resolve(&self, id: i64, resolution: R) -> Option<I> {
        let entry = self.pending.lock().await.remove(&id)?;
        let _ = entry.tx.send(resolution);
        Some(entry.info)
    }

    /// Removes the entry for `id` WITHOUT sending a resolution: the dropped sender
    /// makes the blocked caller observe `RecvError`. Used for deadline / disconnect
    /// cancellation. Returns the removed `info`, or `None` if absent.
    pub async fn remove(&self, id: i64) -> Option<I> {
        self.pending.lock().await.remove(&id).map(|e| e.info)
    }

    /// Snapshot of the `info` for a single pending id, without resolving it.
    pub async fn get(&self, id: i64) -> Option<I> {
        self.pending.lock().await.get(&id).map(|e| e.info.clone())
    }

    /// Snapshot of every pending `info`, in unspecified order.
    pub async fn list(&self) -> Vec<I> {
        self.pending.lock().await.values().map(|e| e.info.clone()).collect()
    }

    /// Drops every entry whose `info` matches `pred` (their senders are dropped, so
    /// the blocked callers observe `RecvError`). Returns the number removed.
    pub async fn remove_where(&self, pred: impl Fn(&I) -> bool) -> usize {
        let mut map = self.pending.lock().await;
        let before = map.len();
        map.retain(|_, e| !pred(&e.info));
        before - map.len()
    }
}

impl<I: Clone, R> Default for PendingRegistry<I, R> {
    fn default() -> Self { Self::new() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn insert_then_resolve_delivers_and_returns_info() {
        let reg: PendingRegistry<i64, String> = PendingRegistry::new();
        let rx = reg.insert(42, 100).await;
        // resolve returns the stored info and unblocks the waiter with the payload.
        assert_eq!(reg.resolve(42, "answer".to_string()).await, Some(100));
        assert_eq!(rx.await.unwrap(), "answer");
        // the entry is gone afterwards.
        assert!(reg.get(42).await.is_none());
    }

    #[tokio::test]
    async fn resolve_unknown_id_is_none() {
        let reg: PendingRegistry<i64, String> = PendingRegistry::new();
        assert_eq!(reg.resolve(1, "x".to_string()).await, None);
    }

    #[tokio::test]
    async fn remove_drops_sender_so_receiver_errors() {
        let reg: PendingRegistry<i64, String> = PendingRegistry::new();
        let rx = reg.insert(7, 100).await;
        assert_eq!(reg.remove(7).await, Some(100));
        // no resolution was sent — the dropped sender makes the waiter observe RecvError.
        assert!(rx.await.is_err());
    }

    #[tokio::test]
    async fn get_and_list_reflect_pending() {
        let reg: PendingRegistry<i64, String> = PendingRegistry::new();
        let _rx1 = reg.insert(1, 10).await;
        let _rx2 = reg.insert(2, 20).await;
        assert_eq!(reg.get(1).await, Some(10));
        let mut all = reg.list().await;
        all.sort();
        assert_eq!(all, vec![10, 20]);
    }

    #[tokio::test]
    async fn remove_where_filters_and_counts() {
        let reg: PendingRegistry<i64, String> = PendingRegistry::new();
        let _rx_keep = reg.insert(1, 10).await;
        let rx_drop  = reg.insert(2, 20).await;
        // remove every entry whose info is >= 20.
        assert_eq!(reg.remove_where(|info| *info >= 20).await, 1);
        assert!(rx_drop.await.is_err());        // dropped entry's waiter errors
        assert_eq!(reg.get(1).await, Some(10)); // the other entry stays
    }
}
