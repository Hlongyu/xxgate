use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditEvent {
    pub id: Uuid,
    pub request_id: Option<Uuid>,
    pub account_id: Option<Uuid>,
    pub kind: String,
    pub actor: String,
    pub at: DateTime<Utc>,
    pub details: Value,
}

impl AuditEvent {
    pub fn new(
        kind: &str,
        actor: &str,
        request_id: Option<Uuid>,
        account_id: Option<Uuid>,
        details: Value,
    ) -> Self {
        Self {
            id: Uuid::new_v4(),
            request_id,
            account_id,
            kind: kind.into(),
            actor: actor.into(),
            at: Utc::now(),
            details,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestRecord {
    /// Captured at admission; None preserves diagnostics for legacy records.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_log_level: Option<crate::settings::LogLevel>,
    /// Detail-only observation; None means this older record did not capture it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_turn_state: Option<crate::turn_state::TurnStateHeader>,
    #[serde(default)]
    pub client_origin: Option<crate::clients::ClientOrigin>,
    #[serde(default)]
    pub stateless: bool,
    #[serde(default)]
    pub ingress_diagnostics: Option<Value>,
    #[serde(default)]
    pub kind: crate::protocol::RequestKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compaction: Option<crate::protocol::Compaction>,
    // None denotes older records or requests rejected before the mode was known.
    #[serde(default)]
    pub stream: Option<bool>,
    #[serde(default)]
    pub search_price: Option<crate::pricing::SearchPrice>,
    pub id: Uuid,
    pub key_id: Uuid,
    #[serde(default)]
    pub group_id: Option<Uuid>,
    pub client_session_id: String,
    pub client_thread_id: String,
    pub model: String,
    pub provider: String,
    #[serde(default)]
    pub upstream_model: Option<String>,
    /// Latest valid model identifier reported by the upstream response.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_model: Option<String>,
    pub requested_tier: Option<String>,
    #[serde(default)]
    pub reasoning_effort: Option<String>,
    pub account_id: Option<Uuid>,
    pub binding_id: Option<Uuid>,
    pub binding_generation: Option<i64>,
    pub state: String,
    pub created_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    pub queue_ms: Option<u64>,
    pub first_event_ms: Option<u64>,
    pub first_content_ms: Option<u64>,
    pub total_ms: Option<u64>,
    pub upstream_status: Option<u16>,
    #[serde(default)]
    pub upstream_headers_ms: Option<u64>,
    pub upstream_request_id: Option<String>,
    pub error_code: Option<String>,
    pub error_message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream_error: Option<Box<crate::types::UpstreamError>>,
    pub usage: crate::usage::Usage,
    pub valuation: Option<crate::pricing::Valuation>,
    pub config_version: i64,
    pub config_versions: Vec<i64>,
    pub body_bytes: usize,
    pub upstream_attempts: u32,
}

impl RequestRecord {
    pub fn summary_only(&self) -> bool {
        self.request_log_level
            .is_some_and(|level| level != crate::settings::LogLevel::Debug)
            && self.state == "completed"
            && self.error_code.is_none()
            && self.upstream_error.is_none()
            && self.usage.complete
            && self.upstream_attempts <= 1
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{settings::LogLevel, usage::Usage};
    use serde_json::json;

    #[test]
    fn only_clean_successes_at_non_debug_levels_are_summarized() {
        let mut record: RequestRecord = serde_json::from_value(json!({
            "id":Uuid::new_v4(),"key_id":Uuid::new_v4(),"client_session_id":"s",
            "client_thread_id":"t","model":"m","provider":"openai","state":"completed",
            "created_at":Utc::now(),"usage":Usage {complete:true,..Default::default()},
            "config_version":1,"config_versions":[1],"body_bytes":100,"upstream_attempts":1
        }))
        .unwrap();
        assert!(!record.summary_only()); // Legacy records retain their details.
        for level in [LogLevel::Info, LogLevel::Warn, LogLevel::Error] {
            record.request_log_level = Some(level);
            assert!(record.summary_only());
            for state in [
                "queued",
                "inflight",
                "failed",
                "rejected",
                "cancelled",
                "interrupted",
            ] {
                record.state = state.into();
                assert!(!record.summary_only());
            }
            record.state = "completed".into();
            record.upstream_attempts = 2;
            assert!(!record.summary_only());
            record.upstream_attempts = 1;
            record.usage.complete = false;
            assert!(!record.summary_only());
            record.usage.complete = true;
        }
        record.request_log_level = Some(LogLevel::Debug);
        assert!(!record.summary_only());
    }
}
