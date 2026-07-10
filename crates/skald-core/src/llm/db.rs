use anyhow::{Context, Result};
use sqlx::SqlitePool;

use core_api::provider::LlmStrength;
use super::{LlmModelRecord, LlmProviderRecord};

// ── Provider rows ─────────────────────────────────────────────────────────────

#[derive(sqlx::FromRow)]
struct ProviderRow {
    id:          i64,
    name:        String,
    r#type:      String,
    api_key:     Option<String>,
    base_url:    Option<String>,
    description: Option<String>,
}

pub async fn load_all_providers(pool: &SqlitePool) -> Result<Vec<LlmProviderRecord>> {
    let rows = sqlx::query_as::<_, ProviderRow>(
        "SELECT id, name, type, api_key, base_url, description FROM llm_providers WHERE removed_at IS NULL ORDER BY name ASC",
    )
    .fetch_all(pool)
    .await
    .context("llm_providers: load_all")?;

    rows.into_iter().map(provider_row_to_record).collect()
}

pub async fn insert_provider(pool: &SqlitePool, r: &LlmProviderRecord) -> Result<i64> {
    let id = sqlx::query_scalar::<_, i64>(
        "INSERT INTO llm_providers (name, type, api_key, base_url, description)
         VALUES (?1, ?2, ?3, ?4, ?5)
         RETURNING id",
    )
    .bind(&r.name)
    .bind(&r.provider)
    .bind(&r.api_key)
    .bind(&r.base_url)
    .bind(&r.description)
    .fetch_one(pool)
    .await
    .context("llm_providers: insert")?;

    Ok(id)
}

pub async fn update_provider(pool: &SqlitePool, id: i64, r: &LlmProviderRecord) -> Result<()> {
    sqlx::query(
        "UPDATE llm_providers
         SET name=?1, type=?2, api_key=?3, base_url=?4, description=?5
         WHERE id=?6",
    )
    .bind(&r.name)
    .bind(&r.provider)
    .bind(&r.api_key)
    .bind(&r.base_url)
    .bind(&r.description)
    .bind(id)
    .execute(pool)
    .await
    .context("llm_providers: update")?;
    Ok(())
}

pub async fn delete_provider(pool: &SqlitePool, id: i64) -> Result<()> {
    // Cascade soft-delete all models belonging to this provider.
    sqlx::query(
        "UPDATE llm_models SET removed_at = datetime('now') WHERE provider_id = ?1 AND removed_at IS NULL",
    )
    .bind(id)
    .execute(pool)
    .await
    .context("llm_models: cascade soft-delete for provider")?;

    // Remove the API key and mark the provider removed.
    sqlx::query(
        "UPDATE llm_providers SET removed_at = datetime('now'), api_key = NULL WHERE id = ?1",
    )
    .bind(id)
    .execute(pool)
    .await
    .context("llm_providers: soft-delete")?;
    Ok(())
}

// ── Model rows ────────────────────────────────────────────────────────────────

#[derive(sqlx::FromRow)]
struct ModelRow {
    id:               i64,
    provider_id:      i64,
    model_id:         String,
    name:             String,
    strength:         Option<String>,
    scope:            String,
    is_default:       i64,
    priority:         i64,
    extra_params:     Option<String>,
    context_length:   Option<i64>,
    max_output_tokens: Option<i64>,
    knowledge_cutoff: Option<String>,
    capabilities:     String,
    reasoning:        Option<String>,
}

pub async fn load_all_models(pool: &SqlitePool) -> Result<Vec<LlmModelRecord>> {
    let rows = sqlx::query_as::<_, ModelRow>(
        "SELECT id, provider_id, model_id, name, strength, scope, is_default, priority, extra_params,
                context_length, max_output_tokens, knowledge_cutoff, capabilities, reasoning
         FROM llm_models
         WHERE removed_at IS NULL
         ORDER BY priority ASC, name ASC",
    )
    .fetch_all(pool)
    .await
    .context("llm_models: load_all")?;

    rows.into_iter().map(model_row_to_record).collect()
}

pub async fn insert_model(pool: &SqlitePool, r: &LlmModelRecord) -> Result<i64> {
    let scope        = serde_json::to_string(&r.scope)?;
    let extra_params = r.extra_params.as_ref().map(|v| v.to_string());
    let capabilities = serde_json::to_string(&r.capabilities)?;
    let reasoning    = r.reasoning.as_ref().map(|v| v.to_string());
    // A model row is never hard-deleted, only soft-deleted via `removed_at`:
    // history and telemetry name it, and `llm_requests` keeps only the model's
    // name, so the row is what maps that name back to a provider. The unique
    // identity is `name` (also the resolution key), so re-adding a previously
    // removed model with the same alias would collide with the lingering
    // soft-deleted row. Upsert on `name` so that existing row is revived
    // (removed_at cleared) and every field overwritten.
    let id = sqlx::query_scalar::<_, i64>(
        "INSERT INTO llm_models (provider_id, model_id, name, strength, scope, is_default, priority, extra_params,
                                context_length, max_output_tokens, knowledge_cutoff, capabilities, reasoning)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
         ON CONFLICT(name) DO UPDATE SET
             provider_id       = excluded.provider_id,
             model_id          = excluded.model_id,
             strength          = excluded.strength,
             scope             = excluded.scope,
             is_default        = excluded.is_default,
             priority          = excluded.priority,
             extra_params      = excluded.extra_params,
             context_length    = excluded.context_length,
             max_output_tokens = excluded.max_output_tokens,
             knowledge_cutoff  = excluded.knowledge_cutoff,
             capabilities      = excluded.capabilities,
             reasoning         = excluded.reasoning,
             removed_at        = NULL
         RETURNING id",
    )
    .bind(r.provider_id)
    .bind(&r.model_id)
    .bind(&r.name)
    .bind(r.strength.map(strength_str))
    .bind(scope)
    .bind(r.is_default as i64)
    .bind(r.priority as i64)
    .bind(extra_params)
    .bind(r.context_length)
    .bind(r.max_output_tokens)
    .bind(&r.knowledge_cutoff)
    .bind(capabilities)
    .bind(reasoning)
    .fetch_one(pool)
    .await
    .context("llm_models: insert")?;

    Ok(id)
}

pub async fn update_model(pool: &SqlitePool, id: i64, r: &LlmModelRecord) -> Result<()> {
    let scope        = serde_json::to_string(&r.scope)?;
    let extra_params = r.extra_params.as_ref().map(|v| v.to_string());
    let capabilities = serde_json::to_string(&r.capabilities)?;
    let reasoning    = r.reasoning.as_ref().map(|v| v.to_string());
    sqlx::query(
        "UPDATE llm_models
         SET provider_id=?1, model_id=?2, name=?3, strength=?4,
             scope=?5, is_default=?6, priority=?7, extra_params=?8,
             context_length=?9, max_output_tokens=?10, knowledge_cutoff=?11, capabilities=?12,
             reasoning=?13
         WHERE id=?14",
    )
    .bind(r.provider_id)
    .bind(&r.model_id)
    .bind(&r.name)
    .bind(r.strength.map(strength_str))
    .bind(scope)
    .bind(r.is_default as i64)
    .bind(r.priority as i64)
    .bind(extra_params)
    .bind(r.context_length)
    .bind(r.max_output_tokens)
    .bind(&r.knowledge_cutoff)
    .bind(capabilities)
    .bind(reasoning)
    .bind(id)
    .execute(pool)
    .await
    .context("llm_models: update")?;
    Ok(())
}

pub async fn delete_model(pool: &SqlitePool, id: i64) -> Result<()> {
    sqlx::query("UPDATE llm_models SET removed_at = datetime('now') WHERE id = ?1")
        .bind(id)
        .execute(pool)
        .await
        .context("llm_models: soft-delete")?;
    Ok(())
}

/// Update catalog-sourced metadata for a model identified by `provider_id` and `model_id`.
/// Used by the sync logic in `LlmManager::list_provider_models`.
pub async fn update_model_metadata(
    pool:               &SqlitePool,
    provider_id:        i64,
    model_id:           &str,
    context_length:     Option<i64>,
    max_output_tokens:  Option<i64>,
    knowledge_cutoff:   Option<&str>,
    capabilities:       &[String],
) -> Result<()> {
    let caps = serde_json::to_string(capabilities)?;
    sqlx::query(
        "UPDATE llm_models
         SET context_length = COALESCE(?1, context_length),
             max_output_tokens = COALESCE(?2, max_output_tokens),
             knowledge_cutoff = COALESCE(?3, knowledge_cutoff),
             capabilities = ?4
         WHERE provider_id = ?5 AND model_id = ?6 AND removed_at IS NULL",
    )
    .bind(context_length)
    .bind(max_output_tokens)
    .bind(knowledge_cutoff)
    .bind(caps)
    .bind(provider_id)
    .bind(model_id)
    .execute(pool)
    .await
    .context("llm_models: update_model_metadata")?;
    Ok(())
}

pub async fn clear_default(pool: &SqlitePool) -> Result<()> {
    sqlx::query("UPDATE llm_models SET is_default=0")
        .execute(pool)
        .await
        .context("llm_models: clear_default")?;
    Ok(())
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn provider_row_to_record(r: ProviderRow) -> Result<LlmProviderRecord> {
    Ok(LlmProviderRecord {
        id:          r.id,
        name:        r.name,
        provider:    r.r#type,
        api_key:     r.api_key,
        base_url:    r.base_url,
        description: r.description,
    })
}

fn model_row_to_record(r: ModelRow) -> Result<LlmModelRecord> {
    let scope: Vec<String> = serde_json::from_str(&r.scope).unwrap_or_default();
    let extra_params = r.extra_params
        .as_deref()
        .and_then(|s| serde_json::from_str(s).ok());
    let capabilities: Vec<String> = serde_json::from_str(&r.capabilities).unwrap_or_default();
    let reasoning = r.reasoning
        .as_deref()
        .and_then(|s| serde_json::from_str(s).ok());
    Ok(LlmModelRecord {
        id:                r.id,
        provider_id:       r.provider_id,
        model_id:          r.model_id,
        name:              r.name,
        strength:          r.strength.as_deref().and_then(parse_strength),
        scope,
        is_default:        r.is_default != 0,
        priority:          r.priority as i32,
        extra_params,
        context_length:    r.context_length,
        max_output_tokens: r.max_output_tokens,
        knowledge_cutoff:  r.knowledge_cutoff,
        capabilities,
        reasoning,
    })
}


pub fn strength_str(s: LlmStrength) -> &'static str {
    match s {
        LlmStrength::VeryLow  => "very_low",
        LlmStrength::Low      => "low",
        LlmStrength::Average  => "average",
        LlmStrength::High     => "high",
        LlmStrength::VeryHigh => "very_high",
    }
}

fn parse_strength(s: &str) -> Option<LlmStrength> {
    match s {
        "very_low"  => Some(LlmStrength::VeryLow),
        "low"       => Some(LlmStrength::Low),
        "average"   => Some(LlmStrength::Average),
        "high"      => Some(LlmStrength::High),
        "very_high" => Some(LlmStrength::VeryHigh),
        _           => None,
    }
}
