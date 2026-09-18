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
