use super::accounts::{PgStore, decode};
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde_json::{Value, json};
use uuid::Uuid;
use xxgate_core::{Error, Result, quota::QuotaWindow, reports::weekly_quota_sample};

#[derive(sqlx::FromRow)]
struct SpendingTotal {
    label: String,
    cny: String,
    requests: i64,
    unpriced: i64,
}

impl PgStore {
    pub(crate) async fn spending_report(
        &self,
        id: Uuid,
        now: DateTime<Utc>,
        stale_seconds: u64,
    ) -> Result<Value> {
        let rows=sqlx::query("SELECT data FROM quota_snapshots WHERE account_id=$1 AND pool='codex' AND window_minutes=10080 AND observed_at>$2-interval '7 days' AND observed_at<=$2 ORDER BY observed_at DESC,id DESC LIMIT 10000")
            .bind(id).bind(now).fetch_all(&self.pool).await.map_err(|_|Error::storage())?;
        let windows = rows
            .iter()
            .map(decode::<QuotaWindow>)
            .collect::<Result<Vec<_>>>()?;
        let sample = weekly_quota_sample(&windows, now);
        let totals = sqlx::query_as::<_, SpendingTotal>(include_str!("account_spending.sql"))
            .bind(id)
            .bind(now)
            .bind(sample.map(|(first, _)| first.observed_at))
            .bind(sample.map(|(_, last)| last.observed_at))
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
            Ok(json!({"hours":hours,"cny":t.cny,"requests":t.requests,"unpriced":t.unpriced}))
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
