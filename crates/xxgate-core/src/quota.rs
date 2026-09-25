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

    /// Whether this exhausted window is still inside its cooldown period.
    pub fn cooldown_active(&self, now: DateTime<Utc>) -> bool {
        self.disable_reason()
            .is_some_and(|_| self.resets_at.is_none_or(|reset| reset > now))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    fn window(minutes: i64, used: f64, reset: Option<DateTime<Utc>>) -> QuotaWindow {
        QuotaWindow {
            pool: "codex".into(),
            window_minutes: Some(minutes),
            used_percent: used,
            resets_at: reset,
            observed_at: Utc::now(),
            source: "test".into(),
        }
    }

    #[test]
    fn quota_windows_map_to_the_three_cooldowns() {
        assert_eq!(
            window(300, 100.0, None).disable_reason(),
            Some(DisableReason::Quota5hExhausted)
        );
        assert_eq!(
            window(10080, 100.0, None).disable_reason(),
            Some(DisableReason::Quota7dExhausted)
        );
        assert_eq!(
            window(43200, 100.0, None).disable_reason(),
            Some(DisableReason::QuotaExhausted)
        );
    }

    #[test]
    fn cooldown_is_active_only_before_reset() {
        let now = Utc::now();
        assert!(window(300, 100.0, Some(now + Duration::minutes(1))).cooldown_active(now));
        assert!(!window(300, 100.0, Some(now - Duration::minutes(1))).cooldown_active(now));
    }
}
