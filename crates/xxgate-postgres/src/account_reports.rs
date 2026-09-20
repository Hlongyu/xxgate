use super::accounts::{PgStore, decode};
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde_json::{Value, json};
use uuid::Uuid;
use xxgate_core::{
    Error, Result,
    quota::QuotaWindow,
    reports::{quota_cycle, weekly_quota_sample},
};

#[derive(sqlx::FromRow)]
struct SpendingTotal {
    label: String,
    cny: String,
    requests: i64,
    unpriced: i64,
    input_tokens: Option<String>,
    output_tokens: Option<String>,
    missing_tokens: i64,
}

impl PgStore {
    pub(crate) async fn spending_report(
        &self,
        id: Uuid,
        now: DateTime<Utc>,
        stale_seconds: u64,
    ) -> Result<Value> {
        let rows=sqlx::query("SELECT data FROM quota_snapshots WHERE account_id=$1 AND pool='codex' AND window_minutes IN (300,10080) AND observed_at>=$2-interval '7 days' AND observed_at<=$2 ORDER BY observed_at,id")
            .bind(id).bind(now).fetch_all(&self.pool).await.map_err(|_|Error::storage())?;
        let windows = rows
            .iter()
            .map(decode::<QuotaWindow>)
            .collect::<Result<Vec<_>>>()?;
        let sample = weekly_quota_sample(&windows, now);
        let short = quota_cycle(&windows, 300, now);
        let week = quota_cycle(&windows, 10080, now);
        let totals = sqlx::query_as::<_, SpendingTotal>(include_str!("account_spending.sql"))
            .bind(id)
            .bind(now)
            .bind(sample.map(|(first, _)| first.observed_at))
            .bind(sample.map(|(_, last)| last.observed_at))
            .bind(short.as_ref().map(|c| c.starts_at))
            .bind(week.as_ref().map(|c| c.starts_at))
            .fetch_all(&self.pool)
            .await
            .map_err(|_| Error::storage())?;
        let total = |label: &str| {
            totals
                .iter()
                .find(|v| v.label == label)
                .ok_or_else(Error::storage)
        };
        let period = |label: &str, hours: i32| -> Result<Value> {
            let t = total(label)?;
            let cycle = if hours == 5 {
                short.as_ref()
            } else {
                week.as_ref()
            };
            Ok(
                json!({"hours":hours,"status":if cycle.is_some() {"current_cycle"} else {"unknown_cycle"},"starts_at":cycle.map(|c|c.starts_at),"resets_at":cycle.and_then(|c|c.latest.resets_at),"cny":cycle.map(|_|&t.cny),"requests":cycle.map(|_|t.requests),"input_tokens":cycle.and(t.input_tokens.as_ref()),"output_tokens":cycle.and(t.output_tokens.as_ref()),"missing_tokens":t.missing_tokens,"unpriced":t.unpriced}),
            )
        };
        let mut estimate = json!({"status":"insufficient_sample","total_cny":null});
        if let Some((first, last)) = sample {
            // Spark has a separate upstream quota pool, so exclude it from the
            // monetary sample for the main Codex weekly quota.
            let t = total("weekly")?;
            let cny: Decimal = t.cny.parse().map_err(|_| Error::storage())?;
            let count = t.requests;
            let delta = last.used_percent - first.used_percent;
            if count > 0 && cny > Decimal::ZERO {
                let percent = Decimal::from_f64_retain(delta).ok_or_else(Error::storage)?;
                estimate = json!({"status":"estimated","total_cny":(cny*Decimal::from(100)/percent).round_dp(4),"sample_cny":cny,"used_percent_delta":delta,"sample_from":first.observed_at,"sample_to":last.observed_at,"requests":count,"unpriced":t.unpriced,"resets_at":last.resets_at,"sample_stale":(now-last.observed_at).num_seconds()>stale_seconds as i64});
            }
        }
        Ok(
            json!({"last_5h":period("last_5h",5)?,"last_7d":period("last_7d",168)?,"weekly_estimate":estimate}),
        )
    }
}

#[cfg(test)]
#[path = "account_reports_tests.rs"]
mod tests;
