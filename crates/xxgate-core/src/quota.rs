use crate::accounts::DisableReason;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuotaWindow {
    pub pool: String,
    pub window_minutes: Option<i64>,
    pub used_percent: f64,
    pub resets_at: Option<DateTime<Utc>>,
    pub observed_at: DateTime<Utc>,
    pub source: String,
}

impl QuotaWindow {
    pub fn disable_reason(&self) -> Option<DisableReason> {
        if !self.used_percent.is_finite() || self.used_percent < 100.0 {
            return None;
        }
        Some(match self.window_minutes {
            Some(300) => DisableReason::Quota5hExhausted,
            Some(10080) => DisableReason::Quota7dExhausted,
            _ => DisableReason::QuotaExhausted,
        })
    }
}
