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

/// A separate credit balance, not a subscription percentage or reset voucher.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtraCredits {
    pub has_credits: Option<bool>,
    pub unlimited: Option<bool>,
    pub balance: Option<String>,
    pub observed_at: DateTime<Utc>,
    pub source: String,
    #[serde(default)]
    pub blocked: bool,
}

impl ExtraCredits {
    pub fn available(&self, now: DateTime<Utc>, stale_after_secs: u64) -> bool {
        let age = (now - self.observed_at).num_seconds();
        if self.blocked || age < 0 || age as u64 > stale_after_secs {
            return false;
        }
        self.unlimited == Some(true)
            || (self.has_credits == Some(true)
                && self.balance.as_ref().is_none_or(|balance| {
                    balance
                        .parse::<f64>()
                        .is_ok_and(|n| n.is_finite() && n > 0.0)
                }))
    }
}

/// Extra credits only cover the ordinary Codex subscription windows.
/// Model-specific limits and other quota pools retain their original behavior.
pub fn quota_blocked(window: &QuotaWindow, extra_available: bool, now: DateTime<Utc>) -> bool {
    window.cooldown_active(now)
        && !(extra_available
            && window.pool == "codex"
            && matches!(window.window_minutes, Some(300 | 10080)))
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
    fn credits_require_fresh_positive_or_unlimited_balance_and_only_cover_subscription() {
        let now = Utc::now();
        let mut c = ExtraCredits {
            has_credits: Some(true),
            unlimited: Some(false),
            balance: Some("12.5".into()),
            observed_at: now,
            source: "test".into(),
            blocked: false,
        };
        assert!(c.available(now, 300));
        assert!(!c.available(now + Duration::seconds(301), 300));
        assert!(!c.available(now - Duration::seconds(1), 300));
        for balance in ["0", "-1", "NaN", "inf", "invalid"] {
            c.balance = Some(balance.into());
            assert!(!c.available(now, 300));
        }
        c.unlimited = Some(true);
        assert!(c.available(now, 300));
        c.blocked = true;
        assert!(!c.available(now, 300));
        for minutes in [300, 10080] {
            assert!(!quota_blocked(&window(minutes, 100.0, None), true, now));
            assert!(quota_blocked(&window(minutes, 100.0, None), false, now));
        }
        assert!(quota_blocked(&window(43200, 100.0, None), true, now));
        let mut other = window(10080, 100.0, None);
        other.pool = "model_specific".into();
        assert!(quota_blocked(&other, true, now));
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
