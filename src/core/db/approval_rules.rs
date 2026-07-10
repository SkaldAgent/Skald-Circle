use anyhow::Result;
use sqlx::SqlitePool;

use crate::core::approval::{ApprovalRule, NewApprovalRule, RuleAction};

type RawRow = (i64, Option<String>, Option<String>, String, Option<String>, String, Option<String>, i64, Option<String>);

fn from_raw((id, agent_id, source, tool_pattern, path_pattern, action, note, priority, group_id): RawRow)
    -> anyhow::Result<ApprovalRule>
{
    let action: RuleAction = action.parse()?;
    Ok(ApprovalRule { id, agent_id, source, tool_pattern, path_pattern, action, note, priority, group_id })
}

/// Returns all rules ordered by priority ASC (lowest number = evaluated first).
pub async fn list(pool: &SqlitePool) -> Result<Vec<ApprovalRule>> {
    let rows = sqlx::query_as::<_, RawRow>(
        "SELECT id, agent_id, source, tool_pattern, path_pattern, action, note, priority, group_id
         FROM   approval_rules
         ORDER  BY priority ASC, id ASC",
    )
    .fetch_all(pool)
    .await?;

    rows.into_iter().map(from_raw).collect()
}

/// Returns rules applicable to `group_id`: group-specific first, then 'default' as fallback.
/// If `group_id` is `None` or equals `"default"`, only default rules are returned.
pub async fn list_for_group(pool: &SqlitePool, group_id: Option<&str>) -> Result<Vec<ApprovalRule>> {
    let effective = group_id.unwrap_or("default");
    let rows = sqlx::query_as::<_, RawRow>(
        "SELECT id, agent_id, source, tool_pattern, path_pattern, action, note, priority, group_id
         FROM   approval_rules
         WHERE  group_id = ?1 OR group_id = 'default'
         ORDER  BY CASE WHEN group_id = ?1 THEN 0 ELSE 1 END, priority ASC, id ASC",
    )
    .bind(effective)
    .fetch_all(pool)
    .await?;

    rows.into_iter().map(from_raw).collect()
}

/// Inserts a new rule; returns its id.
pub async fn insert(pool: &SqlitePool, r: NewApprovalRule) -> Result<i64> {
    let priority = r.priority.unwrap_or(100);
    let group_id = r.group_id.as_deref().unwrap_or("default");
    let id = sqlx::query_scalar::<_, i64>(
        "INSERT INTO approval_rules (agent_id, source, tool_pattern, path_pattern, action, note, priority, group_id)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)
         RETURNING id",
    )
    .bind(r.agent_id)
    .bind(r.source)
    .bind(r.tool_pattern)
    .bind(r.path_pattern)
    .bind(r.action.as_str())
    .bind(r.note)
    .bind(priority)
    .bind(group_id)
    .fetch_one(pool)
    .await?;
    Ok(id)
}

/// Updates an existing rule by id.
pub async fn update(pool: &SqlitePool, id: i64, r: NewApprovalRule) -> Result<()> {
    let priority = r.priority.unwrap_or(100);
    let group_id = r.group_id.as_deref().unwrap_or("default");
    sqlx::query(
        "UPDATE approval_rules
         SET agent_id = ?, source = ?, tool_pattern = ?, path_pattern = ?, action = ?, note = ?, priority = ?, group_id = ?
         WHERE id = ?",
    )
    .bind(r.agent_id)
    .bind(r.source)
    .bind(r.tool_pattern)
    .bind(r.path_pattern)
    .bind(r.action.as_str())
    .bind(r.note)
    .bind(priority)
    .bind(group_id)
    .bind(id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Deletes a rule by id.
pub async fn delete(pool: &SqlitePool, id: i64) -> Result<()> {
    sqlx::query("DELETE FROM approval_rules WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}
