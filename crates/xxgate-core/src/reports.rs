use crate::{Error, Result};
use chrono::{DateTime, Timelike, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UsageFilter {
    pub account_id: Option<Uuid>,
    pub key_id: Option<Uuid>,
    pub model: Option<String>,
    pub service_tier: Option<String>,
    pub from: Option<DateTime<Utc>>,
    pub to: Option<DateTime<Utc>>,
}
impl UsageFilter {
    pub fn validate(&self) -> Result<()> {
        if self.from.zip(self.to).is_some_and(|(from, to)| from >= to) {
            return Err(Error::invalid("Report start must precede its end"));
        }
        if [self.from, self.to]
            .into_iter()
            .flatten()
            .any(|d| d.minute() != 0 || d.second() != 0 || d.nanosecond() != 0)
        {
            return Err(Error::invalid(
                "Aggregate report boundaries must use whole hours",
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RequestFilter {
    pub client_source: Option<crate::clients::ClientSource>,
    pub kind: Option<crate::protocol::RequestKind>,
    pub account_id: Option<Uuid>,
    pub key_id: Option<Uuid>,
    pub state: Option<String>,
    pub model: Option<String>,
    pub session_id: Option<String>,
    pub id: Option<Uuid>,
    pub from: Option<DateTime<Utc>>,
    pub to: Option<DateTime<Utc>>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ErrorFilter {
    pub from: Option<DateTime<Utc>>,
    pub to: Option<DateTime<Utc>>,
    pub account_id: Option<Uuid>,
    pub key_id: Option<Uuid>,
    pub model: Option<String>,
    pub kind: Option<crate::protocol::RequestKind>,
    pub state: Option<String>,
    pub code: Option<String>,
    pub stage: Option<String>,
    pub upstream_status: Option<u16>,
    #[serde(default)]
    pub upstream_missing: bool,
    pub cause: Option<String>,
    pub param: Option<String>,
    pub session_id: Option<String>,
    pub request_id: Option<Uuid>,
    pub q: Option<String>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

impl ErrorFilter {
    pub fn window(&self, now: DateTime<Utc>) -> Result<(DateTime<Utc>, DateTime<Utc>)> {
        let to = self.to.unwrap_or(now);
        let from = match self.from {
            Some(from) => from,
            None => to
                .checked_sub_signed(chrono::Duration::hours(24))
                .ok_or_else(|| Error::invalid("Invalid error report end time"))?,
        };
        if from >= to || to - from > chrono::Duration::days(31) {
            return Err(Error::invalid(
                "Error report range must be positive and at most 31 days",
            ));
        }
        if self
            .state
            .as_deref()
            .is_some_and(|v| !["failed", "rejected", "interrupted", "cancelled"].contains(&v))
            || self
                .upstream_status
                .is_some_and(|v| !(100..=599).contains(&v))
            || (self.upstream_missing && self.upstream_status.is_some())
            || self.limit.is_some_and(|v| !(1..=100).contains(&v))
            || self.offset.is_some_and(|v| v < 0)
        {
            return Err(Error::invalid("Invalid error report filter"));
        }
        for value in [
            &self.model,
            &self.code,
            &self.stage,
            &self.cause,
            &self.param,
            &self.session_id,
            &self.q,
        ]
        .into_iter()
        .flatten()
        {
            if value.len() > 512 || value.chars().any(char::is_control) {
                return Err(Error::invalid(
                    "Error filters must be at most 512 bytes without control characters",
                ));
            }
        }
        Ok((from, to))
    }
}

pub fn weekly_quota_sample(
    windows: &[crate::quota::QuotaWindow],
    now: DateTime<Utc>,
) -> Option<(&crate::quota::QuotaWindow, &crate::quota::QuotaWindow)> {
    let mut points: Vec<_> = windows
        .iter()
        .filter(|w| {
            w.pool == "codex"
                && w.window_minutes == Some(10080)
                && w.used_percent.is_finite()
                && (0.0..=100.0).contains(&w.used_percent)
                && w.observed_at <= now
        })
        .collect();
    points.sort_by_key(|w| w.observed_at);
    let latest = *points.last()?;
    let reset = latest.resets_at.filter(|r| *r > now)?;
    let mut first = latest;
    let mut previous = None;
    for point in points.into_iter().filter(|p| {
        p.observed_at >= reset - chrono::Duration::days(7)
            && p.resets_at
                .is_some_and(|r| (r - reset).num_seconds().abs() <= 60)
    }) {
        if previous.is_none_or(|used| point.used_percent < used) {
            first = point;
        }
        previous = Some(point.used_percent);
    }
    (latest.used_percent - first.used_percent >= 1.0).then_some((first, latest))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::quota::QuotaWindow;
    #[test]
    fn weekly_sample_requires_a_current_consistent_cycle_and_a_measurable_delta() {
        let now = Utc::now();
        let reset = now + chrono::Duration::days(3);
        let window = |percent, minutes, reset| QuotaWindow {
            pool: "codex".into(),
            window_minutes: Some(10080),
            used_percent: percent,
            resets_at: Some(reset),
            observed_at: now - chrono::Duration::minutes(minutes),
            source: "test".into(),
        };
        let mut windows = vec![
            window(20.0, 60, reset),
            window(100.0, 30, reset - chrono::Duration::days(1)),
            window(30.0, 0, reset),
        ];
        let (a, b) = weekly_quota_sample(&windows, now).unwrap();
        assert_eq!((a.used_percent, b.used_percent), (20.0, 30.0));
        assert!(weekly_quota_sample(&windows, reset).is_none());
        windows.insert(2, window(10.0, 10, reset));
        let (a, b) = weekly_quota_sample(&windows, now).unwrap();
        assert_eq!((a.used_percent, b.used_percent), (10.0, 30.0));
        assert!(weekly_quota_sample(&windows, now + chrono::Duration::hours(1)).is_some());
        assert!(
            weekly_quota_sample(&[window(30.0, 5, reset), window(30.0, 0, reset)], now).is_none()
        );
    }
}
