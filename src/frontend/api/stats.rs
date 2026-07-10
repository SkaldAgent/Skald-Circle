use axum::{
    extract::{Query, State},
    response::IntoResponse,
    Json,
};
use serde::{Deserialize, Serialize};

use std::sync::Arc;
use skald_core::skald::Skald;
use super::ApiError;

#[derive(Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum StatsRange {
    Hour,
    Day,
    #[default]
    Week,
    Month,
}

#[derive(Deserialize)]
pub struct StatsQuery {
    pub range: Option<StatsRange>,
}

#[derive(Serialize)]
pub struct DailyStats {
    pub day:               String,
    pub requests:          i64,
    pub input_tokens:      i64,
    pub output_tokens:     i64,
    pub cache_read_tokens: i64,
    pub avg_duration_ms:   f64,
}

#[derive(Serialize)]
pub struct ModelStats {
    pub model_name: String,
    pub requests:   i64,
}

#[derive(Serialize)]
pub struct LlmStatsResponse {
    pub daily:  Vec<DailyStats>,
    pub models: Vec<ModelStats>,
}

const SQL_DAILY_HOUR: &str =
    "SELECT strftime('%H:%M', created_at, 'localtime')        AS day,
            COUNT(*)                                          AS requests,
            COALESCE(SUM(input_tokens),      0)               AS input_tokens,
            COALESCE(SUM(output_tokens),     0)               AS output_tokens,
            COALESCE(SUM(cache_read_tokens), 0)               AS cache_read_tokens,
            AVG(duration_ms)                                  AS avg_duration_ms
     FROM llm_requests
     WHERE created_at >= datetime('now', ?)
     GROUP BY strftime('%H:%M', created_at, 'localtime')
     ORDER BY day ASC";

const SQL_DAILY_HOUR_BUCKET: &str =
    "SELECT strftime('%m-%d %H:00', created_at, 'localtime')  AS day,
            COUNT(*)                                          AS requests,
            COALESCE(SUM(input_tokens),      0)               AS input_tokens,
            COALESCE(SUM(output_tokens),     0)               AS output_tokens,
            COALESCE(SUM(cache_read_tokens), 0)               AS cache_read_tokens,
            AVG(duration_ms)                                  AS avg_duration_ms
     FROM llm_requests
     WHERE created_at >= datetime('now', ?)
     GROUP BY strftime('%m-%d %H:00', created_at, 'localtime')
     ORDER BY day ASC";

const SQL_DAILY_DATE: &str =
    "SELECT DATE(created_at, 'localtime')                     AS day,
            COUNT(*)                                          AS requests,
            COALESCE(SUM(input_tokens),      0)               AS input_tokens,
            COALESCE(SUM(output_tokens),     0)               AS output_tokens,
            COALESCE(SUM(cache_read_tokens), 0)               AS cache_read_tokens,
            AVG(duration_ms)                                  AS avg_duration_ms
     FROM llm_requests
     WHERE created_at >= datetime('now', ?)
     GROUP BY DATE(created_at, 'localtime')
     ORDER BY day ASC";

const SQL_MODELS: &str =
    "SELECT model_name, COUNT(*) AS requests
     FROM llm_requests
     WHERE created_at >= datetime('now', ?)
     GROUP BY model_name
     ORDER BY requests DESC
     LIMIT 6";

pub async fn llm_stats(
    State(skald): State<Arc<Skald>>,
    Query(params): Query<StatsQuery>,
) -> Result<impl IntoResponse, ApiError> {
    let range = params.range.unwrap_or_default();

    let (window, daily_sql) = match range {
        StatsRange::Hour  => ("-60 minutes", SQL_DAILY_HOUR),
        StatsRange::Day   => ("-24 hours",   SQL_DAILY_HOUR_BUCKET),
        StatsRange::Week  => ("-7 days",     SQL_DAILY_DATE),
        StatsRange::Month => ("-30 days",    SQL_DAILY_DATE),
    };

    let daily = sqlx::query_as::<_, (String, i64, i64, i64, i64, f64)>(daily_sql)
        .bind(window)
        .fetch_all(&**skald.db())
        .await?
        .into_iter()
        .map(|(day, requests, input_tokens, output_tokens, cache_read_tokens, avg_duration_ms)| {
            DailyStats { day, requests, input_tokens, output_tokens, cache_read_tokens, avg_duration_ms }
        })
        .collect::<Vec<_>>();

    let models = sqlx::query_as::<_, (String, i64)>(SQL_MODELS)
        .bind(window)
        .fetch_all(&**skald.db())
        .await?
        .into_iter()
        .map(|(model_name, requests)| ModelStats { model_name, requests })
        .collect::<Vec<_>>();

    Ok(Json(LlmStatsResponse { daily, models }))
}
