//! Accessor for `memory_docs` — the backing store of the virtual `memory/`
//! namespace (blueprint §5).
//!
//! The **pool is the namespace**: a user pool holds that user's private notes
//! (`memory/{userid}`), the system pool holds shared notes (`memory/shared`).
//! Callers pass a `path` already stripped of the `memory/…` prefix — the file
//! it lands in decides the namespace, the row keeps only the tail. `path` is
//! UNIQUE, so [`upsert`] is the single write path for both create and edit, and
//! the `memory_docs_fts` triggers keep the full-text index in step underneath.

use anyhow::Result;
use sqlx::SqlitePool;

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct MemoryDoc {
    pub id:         i64,
    pub path:       String,
    pub content:    String,
    pub created_at: String,
    pub updated_at: String,
}

/// One row of a directory-style listing: metadata only, no `content` body.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct MemoryEntry {
    pub path:       String,
    pub updated_at: String,
}

/// One full-text hit: the matching note's path and a highlighted excerpt.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct MemoryHit {
    pub path:    String,
    pub snippet: String,
}

/// A directory listing row carrying cheap size metadata. `line_count` and
/// `byte_len` are computed in SQL (`LENGTH` / newline count) so the note body
/// never leaves the database.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct MemoryEntryMeta {
    pub path:       String,
    pub line_count: i64,
    pub byte_len:   i64,
}

const SELECT: &str = "SELECT id, path, content, created_at, updated_at FROM memory_docs";

/// Fetch one note by its exact path.
pub async fn get(pool: &SqlitePool, path: &str) -> Result<Option<MemoryDoc>> {
    let row = sqlx::query_as::<_, MemoryDoc>(sqlx::AssertSqlSafe(format!("{SELECT} WHERE path = ?")))
        .bind(path)
        .fetch_optional(pool)
        .await?;
    Ok(row)
}

/// Create the note at `path`, or overwrite it if it already exists. `created_at`
/// survives an overwrite; `updated_at` is bumped. Returns the stored row.
pub async fn upsert(pool: &SqlitePool, path: &str, content: &str) -> Result<MemoryDoc> {
    sqlx::query(
        "INSERT INTO memory_docs (path, content)
         VALUES (?, ?)
         ON CONFLICT(path) DO UPDATE SET
             content    = excluded.content,
             updated_at = datetime('now')",
    )
    .bind(path)
    .bind(content)
    .execute(pool)
    .await?;

    let row = sqlx::query_as::<_, MemoryDoc>(sqlx::AssertSqlSafe(format!("{SELECT} WHERE path = ?")))
        .bind(path)
        .fetch_one(pool)
        .await?;
    Ok(row)
}

/// List notes whose path starts with `prefix` (pass `""` for all), most recently
/// edited first. Metadata only — the `content` body is not loaded.
pub async fn list(pool: &SqlitePool, prefix: &str) -> Result<Vec<MemoryEntry>> {
    // Escaped LIKE prefix: a literal `%`/`_` in the caller's path must match as
    // itself, not as a wildcard. `\` is the escape character.
    let pattern = format!("{}%", escape_like(prefix));
    let rows = sqlx::query_as::<_, MemoryEntry>(
        "SELECT path, updated_at FROM memory_docs
         WHERE path LIKE ? ESCAPE '\\'
         ORDER BY updated_at DESC",
    )
    .bind(pattern)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Like [`list`], but each row also carries a line count and byte length,
/// computed in SQL so the body is never transferred. Line count matches the
/// on-disk convention: an empty note is 0 lines, otherwise newline-count + 1.
pub async fn list_with_metadata(pool: &SqlitePool, prefix: &str) -> Result<Vec<MemoryEntryMeta>> {
    let pattern = format!("{}%", escape_like(prefix));
    let rows = sqlx::query_as::<_, MemoryEntryMeta>(
        "SELECT path,
                CASE WHEN content = '' THEN 0
                     ELSE LENGTH(content) - LENGTH(REPLACE(content, char(10), '')) + 1
                END AS line_count,
                LENGTH(CAST(content AS BLOB)) AS byte_len
         FROM memory_docs
         WHERE path LIKE ? ESCAPE '\\'
         ORDER BY updated_at DESC",
    )
    .bind(pattern)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Full-text search over note bodies and paths, best match first. `query` is
/// FTS5 MATCH syntax; `snippet` is a short excerpt of the body with the matched
/// terms wrapped in `[` … `]`.
pub async fn search(pool: &SqlitePool, query: &str, limit: i64) -> Result<Vec<MemoryHit>> {
    let rows = sqlx::query_as::<_, MemoryHit>(
        // `memory_docs_fts` is external-content, so its rowid is `memory_docs.id`;
        // join back for the path, and read the excerpt from content column 1.
        "SELECT d.path AS path,
                snippet(memory_docs_fts, 1, '[', ']', '…', 12) AS snippet
         FROM memory_docs_fts
         JOIN memory_docs d ON d.id = memory_docs_fts.rowid
         WHERE memory_docs_fts MATCH ?
         ORDER BY bm25(memory_docs_fts)
         LIMIT ?",
    )
    .bind(query)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Delete the note at `path`. Returns whether a row was removed.
pub async fn delete(pool: &SqlitePool, path: &str) -> Result<bool> {
    let n = sqlx::query("DELETE FROM memory_docs WHERE path = ?")
        .bind(path)
        .execute(pool)
        .await?
        .rows_affected();
    Ok(n > 0)
}

/// Escapes `%`, `_` and `\` so a caller-supplied string is a literal LIKE prefix.
fn escape_like(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(c, '%' | '_' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// A standalone owner-schema database in a throwaway temp dir. `tag` plus an
    /// atomic counter keep parallel tests from colliding on the same file.
    /// Returns the pool and the dir so the caller can wipe it (SQLite leaves
    /// `-wal`/`-shm` sidecars beside the file).
    async fn owner_pool(tag: &str) -> (SqlitePool, PathBuf) {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir()
            .join(format!("skald-memdocs-{}-{tag}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let pool = crate::db::create_user_pool(&dir.join("owner.db"), None).await.unwrap();
        (pool, dir)
    }

    #[tokio::test]
    async fn upsert_is_create_then_overwrite_and_fts_follows() {
        let (pool, dir) = owner_pool("upsert").await;

        // create
        let doc = upsert(&pool, "notes/spesa.md", "latte e pane").await.unwrap();
        assert_eq!(doc.path, "notes/spesa.md");
        assert_eq!(doc.content, "latte e pane");
        let first_id = doc.id;

        // get by exact path
        assert_eq!(get(&pool, "notes/spesa.md").await.unwrap().unwrap().content, "latte e pane");
        assert!(get(&pool, "notes/altro.md").await.unwrap().is_none());

        // overwrite: same row, new content, created_at preserved
        let doc2 = upsert(&pool, "notes/spesa.md", "latte, pane, uova").await.unwrap();
        assert_eq!(doc2.id, first_id, "upsert must update in place, not insert a new row");
        assert_eq!(doc2.content, "latte, pane, uova");
        assert_eq!(doc2.created_at, doc.created_at, "created_at survives an overwrite");
        assert_eq!(list(&pool, "").await.unwrap().len(), 1, "still one row for that path");

        // FTS follows the update: the removed token is gone, the new one is found
        assert!(search(&pool, "uova", 10).await.unwrap().iter().any(|h| h.path == "notes/spesa.md"));
        assert!(search(&pool, "latte", 10).await.unwrap().iter().any(|h| h.path == "notes/spesa.md"));

        pool.close().await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn list_by_prefix_and_delete_deindexes() {
        let (pool, dir) = owner_pool("list").await;

        upsert(&pool, "notes/spesa.md", "latte pane uova").await.unwrap();
        upsert(&pool, "notes/idee.md",  "un'idea brillante").await.unwrap();
        upsert(&pool, "diary/2026.md",  "oggi e' successo").await.unwrap();

        let notes = list(&pool, "notes/").await.unwrap();
        assert_eq!(notes.len(), 2, "prefix listing is scoped to the subtree");
        assert!(notes.iter().all(|e| e.path.starts_with("notes/")));
        assert_eq!(list(&pool, "").await.unwrap().len(), 3, "empty prefix lists everything");

        // delete removes the row and de-indexes it from FTS
        assert!(delete(&pool, "notes/spesa.md").await.unwrap());
        assert!(get(&pool, "notes/spesa.md").await.unwrap().is_none());
        assert!(search(&pool, "uova", 10).await.unwrap().is_empty(), "delete must de-index");
        assert!(!delete(&pool, "notes/spesa.md").await.unwrap(), "second delete is a no-op");

        pool.close().await;
        let _ = std::fs::remove_dir_all(&dir);
    }
}
