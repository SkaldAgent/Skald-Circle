//! Seeds the two structural notes every memory store needs — `index.md` and
//! `log.md` — so the wiki has a skeleton before anything is written to it.
//!
//! Why this exists: the agents' memory schema (`agents/common/memory-wiki.md`)
//! tells the model to keep both files in sync, and `meta.json` injects
//! `index.md` into every chat turn. On a fresh store neither file exists, an
//! injection of a missing note silently resolves to nothing, and the model is
//! left to invent the structure — or not. Seeding costs two SELECTs at boot and
//! removes that coin flip.
//!
//! The bodies are deliberately **minimal**. `index.md` rides in the system
//! prompt of every turn, so it must not restate the schema — the schema is
//! already in the prompt, and saying it twice is how two sources of truth start
//! to drift.
//!
//! Called for the shared store at boot (idempotent, so an existing instance
//! gets it too) and for a private store when its database is created.

use anyhow::Result;
use sqlx::SqlitePool;

use crate::db::memory_docs;

/// The catalogue: one line per note. Injected into every chat turn.
const INDEX_PATH: &str = "index.md";
const INDEX_SEED: &str = "# Index\n\n_No notes yet._\n";

/// The append-only history. Never injected — read on demand.
const LOG_PATH: &str = "log.md";
const LOG_SEED: &str = "# History\n";

/// The assistant's front page for its owner. Private stores only: shared memory
/// has no single "user" it is about.
const USER_PATH: &str = "user.md";
const USER_SEED: &str = "# User\n\n_Nothing recorded yet._\n";

/// The two notes every store has, private or shared.
const COMMON: &[(&str, &str)] = &[(INDEX_PATH, INDEX_SEED), (LOG_PATH, LOG_SEED)];

/// Scaffolds a **private** store: the two common notes plus `user.md`.
///
/// `user.md` is seeded even though only the `assistant` agent injects it, and
/// seeded *empty* rather than left absent: a missing note resolves to nothing
/// at injection time, so the model cannot tell "no facts yet" from "this
/// mechanism is not running". An explicit `_Nothing recorded yet._` is a signal
/// it can act on — the same convention as the `unknown` lines in the user
/// profile block.
pub async fn seed_private(pool: &SqlitePool) -> Result<()> {
    write_missing(pool, COMMON).await?;
    write_missing(pool, &[(USER_PATH, USER_SEED)]).await
}

/// Scaffolds the **shared** store: the two common notes only.
pub async fn seed_shared(pool: &SqlitePool) -> Result<()> {
    write_missing(pool, COMMON).await
}

/// Creates each note that is absent, leaving existing ones untouched — so this
/// is safe to run on every boot and can never overwrite a real index, truncate a
/// history, or wipe a curated `user.md`.
async fn write_missing(pool: &SqlitePool, notes: &[(&str, &str)]) -> Result<()> {
    for &(path, body) in notes {
        if memory_docs::get(pool, path).await?.is_none() {
            memory_docs::upsert(pool, path, body).await?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    async fn owner_pool(tag: &str) -> (SqlitePool, PathBuf) {
        let dir = std::env::temp_dir()
            .join(format!("skald-scaffold-{tag}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let pool = crate::db::create_user_pool(&dir.join("owner.db"), None).await.unwrap();
        (pool, dir)
    }

    #[tokio::test]
    async fn private_seed_creates_all_three_notes_and_never_clobbers_them() {
        let (pool, dir) = owner_pool("seed").await;

        seed_private(&pool).await.unwrap();
        assert_eq!(memory_docs::get(&pool, "index.md").await.unwrap().unwrap().content, INDEX_SEED);
        assert_eq!(memory_docs::get(&pool, "log.md").await.unwrap().unwrap().content, LOG_SEED);
        assert_eq!(memory_docs::get(&pool, "user.md").await.unwrap().unwrap().content, USER_SEED);

        // Real content lands on top…
        memory_docs::append(&pool, "log.md", "2026-07-26 | ADD | anna | casa.md | created\n")
            .await.unwrap();
        memory_docs::upsert(&pool, "index.md", "# Index\n\n- casa.md — the house\n").await.unwrap();
        memory_docs::upsert(&pool, "user.md", "# User\n\n- Prefers Italian\n").await.unwrap();

        // …and a second boot must not undo it. This is the whole safety property:
        // the seed runs on every start, over stores that are already in use.
        seed_private(&pool).await.unwrap();
        let log = memory_docs::get(&pool, "log.md").await.unwrap().unwrap().content;
        assert!(log.contains("casa.md | created"), "seed truncated the history: {log:?}");
        let index = memory_docs::get(&pool, "index.md").await.unwrap().unwrap().content;
        assert!(index.contains("the house"), "seed overwrote the index: {index:?}");
        let user = memory_docs::get(&pool, "user.md").await.unwrap().unwrap().content;
        assert!(user.contains("Prefers Italian"), "seed wiped the front page: {user:?}");

        pool.close().await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Shared memory is nobody's front page — `user.md` there would be a note
    /// about "the user" in a store that has no single user.
    #[tokio::test]
    async fn shared_seed_omits_the_user_front_page() {
        let (pool, dir) = owner_pool("shared").await;

        seed_shared(&pool).await.unwrap();
        assert!(memory_docs::get(&pool, "index.md").await.unwrap().is_some());
        assert!(memory_docs::get(&pool, "log.md").await.unwrap().is_some());
        assert!(
            memory_docs::get(&pool, "user.md").await.unwrap().is_none(),
            "shared memory must not get a user front page",
        );

        pool.close().await;
        let _ = std::fs::remove_dir_all(&dir);
    }
}
