use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResetCredit {
    pub id: String,
    pub reset_type: String,
    pub status: String,
    pub granted_at: DateTime<Utc>,
    pub expires_at: Option<DateTime<Utc>>,
    pub title: Option<String>,
    pub description: Option<String>,
}

impl ResetCredit {
    pub fn available(&self, now: DateTime<Utc>) -> bool {
        self.status == "available"
            && self.reset_type == "codex_rate_limits"
            && self.expires_at.is_none_or(|at| at > now)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResetCredits {
    pub available_count: u64,
    pub credits: Vec<ResetCredit>,
    #[serde(default = "Utc::now")]
    pub observed_at: DateTime<Utc>,
}

impl ResetCredits {
    pub fn sort(&mut self) {
        self.credits.sort_by_key(|c| {
            (
                c.expires_at.is_none(),
                c.expires_at,
                c.granted_at,
                c.id.clone(),
            )
        });
    }

    pub fn next(&self, now: DateTime<Utc>) -> Option<&ResetCredit> {
        self.credits
            .iter()
            .filter(|c| c.available(now))
            .min_by_key(|c| (c.expires_at.is_none(), c.expires_at, c.granted_at, &c.id))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ResetCode {
    Reset,
    NothingToReset,
    NoCredit,
    AlreadyRedeemed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResetResult {
    pub code: ResetCode,
    #[serde(default)]
    pub windows_reset: u64,
}

// Written before dispatch. Unknown outcomes keep this exact request/credit pair
// across process restarts; a retry must never choose a new credit.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResetOperation {
    pub id: Uuid,
    pub account_id: Uuid,
    pub credit_id: String,
    pub created_at: DateTime<Utc>,
    pub result: Option<ResetResult>,
}
