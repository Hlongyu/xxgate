use super::accounts::{PgStore, decode};
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde_json::{Value, json};
use uuid::Uuid;
use xxgate_core::{
    Error, Result,
    quota::QuotaWindow,
    reports::{QuotaCycle, quota_cycle, quota_sample, weekly_quota_sample},
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
        let rows=sqlx::query("SELECT data FROM quota_snapshots WHERE account_id=$1 AND pool='codex' AND window_minutes IN (300,10080,43200) AND observed_at>=$2-make_interval(mins=>window_minutes::int) AND observed_at<=$2 ORDER BY observed_at,id")
            .bind(id).bind(now).fetch_all(&self.pool).await.map_err(|_|Error::storage())?;
        let (complete_since, created_at): (DateTime<Utc>, Option<DateTime<Utc>>) =
            sqlx::query_as("SELECT c.complete_since,(a.data->>'created_at')::timestamptz FROM account_spending_coverage c LEFT JOIN accounts a ON a.id=$1 WHERE c.singleton")
                .bind(id).fetch_one(&self.pool).await
                .map_err(|_|Error::storage())?;
        let history_complete =
            |from: DateTime<Utc>| from.max(created_at.unwrap_or(from)) >= complete_since;
        let windows = rows
            .iter()
            .map(decode::<QuotaWindow>)
            .collect::<Result<Vec<_>>>()?;
        let sample = weekly_quota_sample(&windows, now);
        let short = quota_cycle(&windows, 300, now);
        let week = quota_cycle(&windows, 10080, now);
        let month = quota_cycle(&windows, 43200, now);
        let monthly_sample = quota_sample(&windows, 43200, now);
        let totals = sqlx::query_as::<_, SpendingTotal>(include_str!("account_spending.sql"))
            .bind(id)
            .bind(now)
            .bind(sample.map(|(first, _)| first.observed_at))
            .bind(sample.map(|(_, last)| last.observed_at))
            .bind(short.as_ref().map(|c| c.starts_at))
            .bind(week.as_ref().map(|c| c.starts_at))
            .bind(month.as_ref().map(|c| c.starts_at))
            .bind(monthly_sample.map(|(first, _)| first.observed_at))
            .bind(monthly_sample.map(|(_, last)| last.observed_at))
            .fetch_all(&self.pool)
            .await
            .map_err(|_| Error::storage())?;
        let total = |label: &str| {
            totals
                .iter()
                .find(|v| v.label == label)
                .ok_or_else(Error::storage)
        };
        let period = |label: &str, hours: i32, cycle: Option<&QuotaCycle<'_>>| -> Result<Value> {
            let t = total(label)?;
            Ok(
                json!({"hours":hours,"status":if cycle.is_some() {"current_cycle"} else {"unknown_cycle"},"starts_at":cycle.map(|c|c.starts_at),"resets_at":cycle.and_then(|c|c.latest.resets_at),"history_complete":cycle.map(|c|history_complete(c.starts_at)),"cny":cycle.map(|_|&t.cny),"requests":cycle.map(|_|t.requests),"input_tokens":cycle.and(t.input_tokens.as_ref()),"output_tokens":cycle.and(t.output_tokens.as_ref()),"missing_tokens":t.missing_tokens,"unpriced":t.unpriced}),
            )
        };
        let estimate = |label: &str,
                        minutes: i64,
                        sample: Option<(&QuotaWindow, &QuotaWindow)>|
         -> Result<Value> {
            let Some((first, last)) = sample else {
                return Ok(
                    json!({"status":"insufficient_sample","total_cny":null,"window_minutes":minutes}),
                );
            };
            if !history_complete(first.observed_at) {
                return Ok(
                    json!({"status":"insufficient_history","total_cny":null,"window_minutes":minutes}),
                );
            }
            // Spark has its own quota pool; exclude it from main quota estimates.
            let t = total(label)?;
            let cny: Decimal = t.cny.parse().map_err(|_| Error::storage())?;
            let count = t.requests;
            let delta = last.used_percent - first.used_percent;
            if count > 0 && cny > Decimal::ZERO {
                let percent = Decimal::from_f64_retain(delta).ok_or_else(Error::storage)?;
                return Ok(
                    json!({"status":"estimated","window_minutes":minutes,"total_cny":(cny*Decimal::from(100)/percent).round_dp(4),"sample_cny":cny,"used_percent_delta":delta,"sample_from":first.observed_at,"sample_to":last.observed_at,"requests":count,"unpriced":t.unpriced,"resets_at":last.resets_at,"sample_stale":(now-last.observed_at).num_seconds()>stale_seconds as i64}),
                );
            }
            Ok(json!({"status":"insufficient_sample","total_cny":null,"window_minutes":minutes}))
        };
        Ok(
            json!({"last_5h":period("last_5h",5,short.as_ref())?,"last_7d":period("last_7d",168,week.as_ref())?,"last_30d":period("last_30d",720,month.as_ref())?,"weekly_estimate":estimate("weekly",10080,sample)?,"monthly_estimate":estimate("monthly",43200,monthly_sample)?}),
        )
    }
}

#[cfg(test)]
#[path = "account_reports_tests.rs"]
mod tests;
