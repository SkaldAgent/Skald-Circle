//! Accessor for `supervision` — the edge that says one person's activity may be
//! read on another's behalf (§0.1).
//!
//! Deliberately anaemic: an edge, two directions, no attributes. It carries no
//! notion of *what* the supervisor may see, because that belongs to whatever
//! reads it — today one background agent, tomorrow a read gate on the reports it
//! writes. Putting "may read conversations" / "may read memory" on the row here
//! would be inventing a permission model before anything asks for one.
//!
//! Registry table, so both foreign keys are real and the cascade is too: deleting
//! either user removes the edge.

use anyhow::Result;
use sqlx::SqlitePool;

/// One edge.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct SupervisionEdge {
    pub subject_user_id:    String,
    pub supervisor_user_id: String,
    pub created_at:         String,
}

/// Every user somebody supervises, in a stable order.
///
/// The order matters more than it looks: it is the order a background pass walks
/// its subjects in, and a stable one makes a partial pass (the process died
/// halfway) resume predictably instead of favouring whoever sorts first by
/// accident.
pub async fn subjects(pool: &SqlitePool) -> Result<Vec<String>> {
    let rows = sqlx::query_scalar::<_, String>(
        "SELECT DISTINCT subject_user_id FROM supervision ORDER BY subject_user_id",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Who supervises `subject`, in a stable order.
pub async fn supervisors_of(pool: &SqlitePool, subject: &str) -> Result<Vec<String>> {
    let rows = sqlx::query_scalar::<_, String>(
        "SELECT supervisor_user_id FROM supervision
         WHERE subject_user_id = ?
         ORDER BY supervisor_user_id",
    )
    .bind(subject)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Whom `supervisor` watches, in a stable order.
pub async fn subjects_of(pool: &SqlitePool, supervisor: &str) -> Result<Vec<String>> {
    let rows = sqlx::query_scalar::<_, String>(
        "SELECT subject_user_id FROM supervision
         WHERE supervisor_user_id = ?
         ORDER BY subject_user_id",
    )
    .bind(supervisor)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Is this edge present? The question a future read gate on a report asks.
pub async fn supervises(pool: &SqlitePool, supervisor: &str, subject: &str) -> Result<bool> {
    let n = sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM supervision
         WHERE supervisor_user_id = ? AND subject_user_id = ?",
    )
    .bind(supervisor)
    .bind(subject)
    .fetch_one(pool)
    .await?;
    Ok(n > 0)
}

/// Every edge, for an admin listing.
pub async fn list(pool: &SqlitePool) -> Result<Vec<SupervisionEdge>> {
    let rows = sqlx::query_as::<_, SupervisionEdge>(
        "SELECT subject_user_id, supervisor_user_id, created_at FROM supervision
         ORDER BY subject_user_id, supervisor_user_id",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Add an edge. Idempotent on the primary key.
///
/// Refuses a self-edge: supervising yourself would make every subject their own
/// supervisor, which is not a special case anyone wants — it is the pass reading
/// its own runtime's data and reporting it to itself.
pub async fn add(pool: &SqlitePool, subject: &str, supervisor: &str) -> Result<()> {
    if subject == supervisor {
        anyhow::bail!("a user cannot supervise themselves");
    }
    sqlx::query(
        "INSERT INTO supervision (subject_user_id, supervisor_user_id)
         VALUES (?, ?)
         ON CONFLICT(subject_user_id, supervisor_user_id) DO NOTHING",
    )
    .bind(subject)
    .bind(supervisor)
    .execute(pool)
    .await?;
    Ok(())
}

/// Remove an edge. Returns whether one was there.
pub async fn remove(pool: &SqlitePool, subject: &str, supervisor: &str) -> Result<bool> {
    let n = sqlx::query(
        "DELETE FROM supervision WHERE subject_user_id = ? AND supervisor_user_id = ?",
    )
    .bind(subject)
    .bind(supervisor)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(n > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A registry pool with two users to hang edges off — the FKs are enforced,
    /// so the rows have to exist.
    async fn registry() -> SqlitePool {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        crate::db::create_registry_tables(&pool).await.unwrap();
        sqlx::query("INSERT INTO roles (id, label, permission_group) VALUES ('member', 'Member', 'default')")
            .execute(&pool).await.unwrap();
        for (id, name) in [("u-anna", "anna"), ("u-bruno", "bruno"), ("u-kid", "kid")] {
            sqlx::query(
                "INSERT INTO users (id, username, display_name, role_id, encrypted)
                 VALUES (?, ?, ?, 'member', 0)",
            )
            .bind(id).bind(name).bind(name)
            .execute(&pool).await.unwrap();
        }
        pool
    }

    #[tokio::test]
    async fn an_edge_reads_from_both_ends() {
        let pool = registry().await;

        add(&pool, "u-kid", "u-anna").await.unwrap();
        add(&pool, "u-kid", "u-bruno").await.unwrap();
        add(&pool, "u-kid", "u-anna").await.unwrap(); // idempotent

        assert_eq!(subjects(&pool).await.unwrap(), vec!["u-kid"]);
        assert_eq!(supervisors_of(&pool, "u-kid").await.unwrap(), vec!["u-anna", "u-bruno"]);
        assert_eq!(subjects_of(&pool, "u-anna").await.unwrap(), vec!["u-kid"]);
        assert!(supervises(&pool, "u-anna", "u-kid").await.unwrap());
        assert!(!supervises(&pool, "u-kid", "u-anna").await.unwrap(), "the edge is directed");
        assert_eq!(list(&pool).await.unwrap().len(), 2);

        assert!(remove(&pool, "u-kid", "u-anna").await.unwrap());
        assert!(!remove(&pool, "u-kid", "u-anna").await.unwrap());
        assert_eq!(supervisors_of(&pool, "u-kid").await.unwrap(), vec!["u-bruno"]);
        // One supervisor left, so the subject is still watched.
        assert_eq!(subjects(&pool).await.unwrap(), vec!["u-kid"]);
    }

    #[tokio::test]
    async fn a_self_edge_is_refused() {
        let pool = registry().await;
        assert!(add(&pool, "u-anna", "u-anna").await.is_err());
    }

    #[tokio::test]
    async fn deleting_a_user_takes_their_edges_from_both_directions() {
        let pool = registry().await;
        add(&pool, "u-kid", "u-anna").await.unwrap();
        add(&pool, "u-bruno", "u-anna").await.unwrap();

        // The supervisor goes: both edges they were on go with them.
        sqlx::query("DELETE FROM users WHERE id = 'u-anna'").execute(&pool).await.unwrap();
        assert!(list(&pool).await.unwrap().is_empty(), "cascade must clear both directions");
    }
}
