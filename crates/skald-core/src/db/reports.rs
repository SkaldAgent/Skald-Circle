//! Accessor for `reports` — the documents system agents write about a stretch
//! of time (blueprint §13).
//!
//! **The pool is the audience.** Like [`super::memory_docs`], one owner schema
//! backs two homes and the file a row lands in decides who may read it: a user's
//! own encrypted database holds the reports that belong to them, `system.db`
//! holds the instance ones — written about someone, for the people who supervise
//! them. Nothing in here filters by reader, because there is nothing to filter:
//! a subject's tools only ever reach their own pool. The separation is
//! structural, not a predicate someone has to remember to add.
//!
//! Which file a producer writes into falls out of its own scope with no new
//! concept: `AgentScope::PerUser` passes `ctx.pool`, `AgentScope::Instance`
//! passes the registry pool it already holds.
//!
//! **A report is immutable.** It is a snapshot of a window that has closed, so
//! there is no `update`: the only write after [`create`] is [`mark_read`], and
//! even that is once — see its "first reader wins" note.

use anyhow::Result;
use sqlx::SqlitePool;

/// Severity, in ascending order of "someone should look at this". Free text in
/// the column; these are the vocabulary the UI knows how to render.
pub const SEVERITY_INFO:   &str = "info";
pub const SEVERITY_NOTICE: &str = "notice";
pub const SEVERITY_ALERT:  &str = "alert";

/// The report belongs to whoever owns the file it is in — the default, and the
/// only meaningful value inside a `{userid}.db`.
pub const AUDIENCE_OWNER: &str = "owner";
/// An instance report (`system.db`) for the admins.
pub const AUDIENCE_ADMINS: &str = "admins";
/// An instance report for whoever holds a [`super::supervision`] edge over its
/// `subject_user_id` — the audience that is *computed*, not enumerated, so adding
/// a second parent to the edge widens the readership of every past report at once.
pub const AUDIENCE_SUPERVISORS: &str = "supervisors";

/// A report with its body. Use [`ReportSummary`] for listings — the body is the
/// bulk of the row and a list never renders it.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Report {
    pub id:               i64,
    pub kind:             String,
    pub title:            String,
    pub summary:          Option<String>,
    pub body:             String,
    pub severity:         String,
    pub subject_user_id:  Option<String>,
    pub audience:         String,
    pub period_start:     Option<String>,
    pub period_end:       Option<String>,
    pub produced_by:      String,
    pub producer_user_id: Option<String>,
    pub run_id:           Option<i64>,
    pub metadata:         Option<String>,
    pub read_at:          Option<String>,
    pub read_by:          Option<String>,
    pub created_at:       String,
}

/// A listing row: everything but `body`.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ReportSummary {
    pub id:               i64,
    pub kind:             String,
    pub title:            String,
    pub summary:          Option<String>,
    pub severity:         String,
    pub subject_user_id:  Option<String>,
    pub audience:         String,
    pub period_start:     Option<String>,
    pub period_end:       Option<String>,
    pub produced_by:      String,
    pub producer_user_id: Option<String>,
    pub run_id:           Option<i64>,
    pub metadata:         Option<String>,
    pub read_at:          Option<String>,
    pub read_by:          Option<String>,
    pub created_at:       String,
}

/// The fields a producer supplies. `kind`, `title`, `body` and `produced_by` are
/// the ones with no sensible default; everything else has one.
#[derive(Debug, Clone)]
pub struct NewReport<'a> {
    /// Producer-declared type — data, never an enum (§0.1). Groups the UI.
    pub kind:             &'a str,
    pub title:            &'a str,
    /// One line for lists and for the notification that announces it.
    pub summary:          Option<&'a str>,
    /// Markdown.
    pub body:             &'a str,
    pub severity:         &'a str,
    /// Who the report is about. `None` for a report about nobody in particular.
    pub subject_user_id:  Option<&'a str>,
    pub audience:         &'a str,
    /// The window covered, ISO-8601. Both `None` for a point-in-time report.
    pub period_start:     Option<&'a str>,
    pub period_end:       Option<&'a str>,
    /// The system agent's id.
    pub produced_by:      &'a str,
    /// Whose runtime ran the pass — for an instance report, not the subject.
    pub producer_user_id: Option<&'a str>,
    /// `system_agent_runs.id`. A bare snapshot: for an instance report that row
    /// lives in the acting user's file, not this one.
    pub run_id:           Option<i64>,
    /// JSON counters. Never contents — the body is the only place text belongs.
    pub metadata:         Option<&'a str>,
}

/// Hand-written, not derived, for the same reason `RoleAttrs`'s is: a derived
/// `Default` would leave `severity` and `audience` empty strings, and both are
/// `NOT NULL` columns whose value the UI dispatches on. The defaults are the
/// quiet, narrow ones — informational, and readable only by the file's owner.
impl Default for NewReport<'_> {
    fn default() -> Self {
        Self {
            kind:             "",
            title:            "",
            summary:          None,
            body:             "",
            severity:         SEVERITY_INFO,
            subject_user_id:  None,
            audience:         AUDIENCE_OWNER,
            period_start:     None,
            period_end:       None,
            produced_by:      "",
            producer_user_id: None,
            run_id:           None,
            metadata:         None,
        }
    }
}

/// How to narrow a [`list`]. All-`None` lists everything, newest first.
#[derive(Debug, Clone, Default)]
pub struct ListFilter<'a> {
    pub kind:            Option<&'a str>,
    pub subject_user_id: Option<&'a str>,
    /// Only reports nobody has acknowledged yet.
    pub unread_only:     bool,
    /// Only reports created at or after this ISO timestamp.
    pub since:           Option<&'a str>,
    pub limit:           Option<i64>,
}

const SUMMARY_COLS: &str = "id, kind, title, summary, severity, subject_user_id, audience, \
     period_start, period_end, produced_by, producer_user_id, run_id, metadata, \
     read_at, read_by, created_at";

/// Write a report. Returns its id.
pub async fn create(pool: &SqlitePool, report: &NewReport<'_>) -> Result<i64> {
    let id = sqlx::query_scalar::<_, i64>(
        "INSERT INTO reports
            (kind, title, summary, body, severity, subject_user_id, audience,
             period_start, period_end, produced_by, producer_user_id, run_id, metadata)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
         RETURNING id",
    )
    .bind(report.kind)
    .bind(report.title)
    .bind(report.summary)
    .bind(report.body)
    .bind(report.severity)
    .bind(report.subject_user_id)
    .bind(report.audience)
    .bind(report.period_start)
    .bind(report.period_end)
    .bind(report.produced_by)
    .bind(report.producer_user_id)
    .bind(report.run_id)
    .bind(report.metadata)
    .fetch_one(pool)
    .await?;
    Ok(id)
}

/// Fetch one report, body included.
pub async fn get(pool: &SqlitePool, id: i64) -> Result<Option<Report>> {
    let row = sqlx::query_as::<_, Report>(
        "SELECT id, kind, title, summary, body, severity, subject_user_id, audience,
                period_start, period_end, produced_by, producer_user_id, run_id, metadata,
                read_at, read_by, created_at
         FROM reports WHERE id = ?",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

/// List reports newest first, without their bodies.
///
/// `id DESC` breaks ties: `created_at` has second resolution, and two reports of
/// the same pass land inside one tick often enough that the order would
/// otherwise be whatever SQLite felt like.
pub async fn list(pool: &SqlitePool, filter: &ListFilter<'_>) -> Result<Vec<ReportSummary>> {
    // The SQL text is assembled only from these literals — every caller-supplied
    // value goes through a bind, in the same order the predicates were pushed.
    let mut predicates: Vec<&str> = Vec::new();
    if filter.kind.is_some()            { predicates.push("kind = ?"); }
    if filter.subject_user_id.is_some() { predicates.push("subject_user_id = ?"); }
    if filter.unread_only               { predicates.push("read_at IS NULL"); }
    if filter.since.is_some()           { predicates.push("created_at >= ?"); }

    let mut sql = format!("SELECT {SUMMARY_COLS} FROM reports");
    if !predicates.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&predicates.join(" AND "));
    }
    sql.push_str(" ORDER BY created_at DESC, id DESC");
    if filter.limit.is_some() {
        sql.push_str(" LIMIT ?");
    }

    let mut query = sqlx::query_as::<_, ReportSummary>(sqlx::AssertSqlSafe(sql));
    if let Some(kind)    = filter.kind            { query = query.bind(kind); }
    if let Some(subject) = filter.subject_user_id { query = query.bind(subject); }
    if let Some(since)   = filter.since           { query = query.bind(since); }
    if let Some(limit)   = filter.limit           { query = query.bind(limit); }

    Ok(query.fetch_all(pool).await?)
}

/// How many reports nobody has acknowledged — the badge count.
pub async fn unread_count(pool: &SqlitePool) -> Result<i64> {
    let n = sqlx::query_scalar::<_, i64>("SELECT count(*) FROM reports WHERE read_at IS NULL")
        .fetch_one(pool)
        .await?;
    Ok(n)
}

/// Acknowledge a report on behalf of `user_id`. Returns whether this call is the
/// one that marked it.
///
/// **First reader wins, and that is the semantics, not an optimisation.** An
/// instance report can have several readers (two admins); an alert about the
/// same evening is one thing to deal with, dealt with once. The `read_at IS
/// NULL` guard makes the write idempotent and keeps `read_by` pointing at
/// whoever actually took it, instead of whoever opened it last.
pub async fn mark_read(pool: &SqlitePool, id: i64, user_id: &str) -> Result<bool> {
    let n = sqlx::query(
        "UPDATE reports SET read_at = datetime('now'), read_by = ?
         WHERE id = ? AND read_at IS NULL",
    )
    .bind(user_id)
    .bind(id)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(n > 0)
}

/// Delete a report. Returns whether a row was removed.
pub async fn delete(pool: &SqlitePool, id: i64) -> Result<bool> {
    let n = sqlx::query("DELETE FROM reports WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?
        .rows_affected();
    Ok(n > 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// A standalone owner-schema database in a throwaway temp dir, as in
    /// `memory_docs`: `tag` plus a counter keep parallel tests off one file.
    async fn owner_pool(tag: &str) -> (SqlitePool, PathBuf) {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir()
            .join(format!("skald-reports-{}-{tag}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let pool = crate::db::create_user_pool(&dir.join("owner.db"), None).await.unwrap();
        (pool, dir)
    }

    #[tokio::test]
    async fn create_stores_every_field_and_defaults_the_rest() {
        let (pool, dir) = owner_pool("create").await;

        // The minimum a producer must supply: the defaults fill the rest.
        let bare = create(&pool, &NewReport {
            kind:        "usage-digest",
            title:       "Your week with the assistant",
            body:        "You asked for a calendar three times and got nowhere.",
            produced_by: "usage-digest",
            ..Default::default()
        }).await.unwrap();

        let got = get(&pool, bare).await.unwrap().unwrap();
        assert_eq!(got.severity, SEVERITY_INFO, "a bare report is informational");
        assert_eq!(got.audience, AUDIENCE_OWNER, "...and readable only by its file's owner");
        assert!(got.subject_user_id.is_none());
        assert!(got.read_at.is_none(), "a fresh report is unread");
        assert!(!got.created_at.is_empty());

        // A full instance report: subject and run_id are bare snapshots, so
        // neither has to exist anywhere in this file.
        let full = create(&pool, &NewReport {
            kind:             "conversation-review",
            title:            "Something to look at",
            summary:          Some("one line for the notification"),
            body:             "# Detail\n\nnarrated, not quoted.",
            severity:         SEVERITY_ALERT,
            subject_user_id:  Some("u-nobody"),
            audience:         AUDIENCE_ADMINS,
            period_start:     Some("2026-07-28T00:00:00Z"),
            period_end:       Some("2026-07-29T00:00:00Z"),
            produced_by:      "conversation-review",
            producer_user_id: Some("u-someone-else"),
            run_id:           Some(4242),
            metadata:         Some(r#"{"sessions_scanned":7}"#),
        }).await.unwrap();

        let got = get(&pool, full).await.unwrap().unwrap();
        assert_eq!(got.severity, SEVERITY_ALERT);
        assert_eq!(got.audience, AUDIENCE_ADMINS);
        assert_eq!(got.subject_user_id.as_deref(), Some("u-nobody"));
        assert_eq!(got.producer_user_id.as_deref(), Some("u-someone-else"));
        assert_eq!(got.run_id, Some(4242));
        assert_eq!(got.period_end.as_deref(), Some("2026-07-29T00:00:00Z"));
        assert!(got.body.starts_with("# Detail"));

        assert!(get(&pool, 9999).await.unwrap().is_none());

        pool.close().await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn list_narrows_and_orders_newest_first() {
        let (pool, dir) = owner_pool("list").await;

        let mk = |kind: &'static str, subject: Option<&'static str>| {
            let pool = pool.clone();
            async move {
                create(&pool, &NewReport {
                    kind,
                    title: "t",
                    body: "b",
                    subject_user_id: subject,
                    produced_by: "agent",
                    ..Default::default()
                }).await.unwrap()
            }
        };

        let first  = mk("usage-digest", None).await;
        let second = mk("conversation-review", Some("u-kid")).await;
        let third  = mk("conversation-review", Some("u-other")).await;

        // Newest first, with `id` breaking the same-second tie.
        let all = list(&pool, &ListFilter::default()).await.unwrap();
        assert_eq!(all.iter().map(|r| r.id).collect::<Vec<_>>(), vec![third, second, first]);

        let by_kind = list(&pool, &ListFilter {
            kind: Some("conversation-review"), ..Default::default()
        }).await.unwrap();
        assert_eq!(by_kind.iter().map(|r| r.id).collect::<Vec<_>>(), vec![third, second]);

        let by_subject = list(&pool, &ListFilter {
            subject_user_id: Some("u-kid"), ..Default::default()
        }).await.unwrap();
        assert_eq!(by_subject.len(), 1);
        assert_eq!(by_subject[0].id, second);

        // Two filters compose, and `limit` applies after the ordering.
        let both = list(&pool, &ListFilter {
            kind: Some("conversation-review"),
            subject_user_id: Some("u-other"),
            ..Default::default()
        }).await.unwrap();
        assert_eq!(both.len(), 1);
        assert_eq!(both[0].id, third);

        let capped = list(&pool, &ListFilter { limit: Some(2), ..Default::default() }).await.unwrap();
        assert_eq!(capped.iter().map(|r| r.id).collect::<Vec<_>>(), vec![third, second]);

        // `since` is inclusive, and a future timestamp excludes everything.
        assert!(list(&pool, &ListFilter {
            since: Some("2999-01-01T00:00:00Z"), ..Default::default()
        }).await.unwrap().is_empty());

        pool.close().await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Several admins share one instance report; whoever gets there first is the
    /// one who took it, and the count reflects the household, not each reader.
    #[tokio::test]
    async fn acknowledgement_is_shared_and_first_writer_wins() {
        let (pool, dir) = owner_pool("read").await;

        let id = create(&pool, &NewReport {
            kind: "conversation-review", title: "t", body: "b",
            audience: AUDIENCE_ADMINS, produced_by: "agent", ..Default::default()
        }).await.unwrap();
        let other = create(&pool, &NewReport {
            kind: "usage-digest", title: "t2", body: "b", produced_by: "agent", ..Default::default()
        }).await.unwrap();

        assert_eq!(unread_count(&pool).await.unwrap(), 2);
        assert_eq!(list(&pool, &ListFilter { unread_only: true, ..Default::default() })
                   .await.unwrap().len(), 2);

        assert!(mark_read(&pool, id, "u-anna").await.unwrap(), "the first reader takes it");
        assert!(!mark_read(&pool, id, "u-bruno").await.unwrap(), "the second changes nothing");

        let got = get(&pool, id).await.unwrap().unwrap();
        assert_eq!(got.read_by.as_deref(), Some("u-anna"), "read_by keeps whoever took it");
        assert!(got.read_at.is_some());

        assert_eq!(unread_count(&pool).await.unwrap(), 1);
        let unread = list(&pool, &ListFilter { unread_only: true, ..Default::default() })
            .await.unwrap();
        assert_eq!(unread.len(), 1);
        assert_eq!(unread[0].id, other);

        assert!(!mark_read(&pool, 9999, "u-anna").await.unwrap(), "an absent report marks nothing");

        assert!(delete(&pool, id).await.unwrap());
        assert!(!delete(&pool, id).await.unwrap(), "a second delete is a no-op");
        assert!(get(&pool, id).await.unwrap().is_none());

        pool.close().await;
        let _ = std::fs::remove_dir_all(&dir);
    }
}
