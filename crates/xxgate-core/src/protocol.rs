use crate::{
    Error, Result,
    accounts::{Account, Credentials, DisableReason},
    identity::{ClientIdentity, IdentityMap},
    providers::ModelSpec,
    quota::QuotaWindow,
    usage::Usage,
};
use async_trait::async_trait;
use bytes::Bytes;
use futures::Stream;
use http::{HeaderMap, Method};
use serde_json::Value;
use std::pin::Pin;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequestKind {
    #[default]
    Responses,
    Compact,
    Search,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompactionMethod {
    Compact,
    RemoteV2,
}

/// Only operation facts are retained, never the compacted transcript or ciphertext.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Compaction {
    pub method: CompactionMethod,
    /// None until an output item or a complete response has been observed.
    pub output_observed: Option<bool>,
}

#[derive(Default)]
pub struct SearchOptions {
    pub query: Option<String>,
    pub beta: Option<http::HeaderValue>,
    pub client_metadata_present: bool,
}

pub struct ProtocolDocument {
    pub format: &'static str,
    pub value: Value,
}

pub struct GatewayRequest {
    /// Observed for diagnostics only; not part of outbound identity headers.
    pub client_turn_state: crate::turn_state::TurnStateHeader,
    pub client_origin: crate::clients::ClientOrigin,
    pub client_metadata_present: bool,
    /// Only explicitly supplied identity headers; no authentication headers.
    pub identity_headers: HeaderMap,
    /// No stable client identity: scheduling scope exists only for this request.
    pub stateless: bool,
    pub ingress_diagnostics: Option<Value>,
    pub kind: RequestKind,
    pub compaction: Option<Compaction>,
    pub search_options: SearchOptions,
    pub identity: ClientIdentity,
    pub model: String,
    pub stream: bool,
    pub requested_tier: Option<String>,
    pub reasoning_effort: Option<String>,
    pub identifier_inputs: std::collections::BTreeMap<String, String>,
    pub document: ProtocolDocument,
}

#[derive(Clone)]
pub struct PreparedRequest {
    pub method: Method,
    pub url: String,
    pub headers: HeaderMap,
    pub body: Bytes,
    pub account_id: Option<Uuid>,
    pub profile_version: i64,
    pub tls_backend: String,
}

/// Counts only: never persist removed ciphertext, summaries, or input content.
#[derive(Debug, Default, serde::Serialize)]
pub struct EncryptedContentRecovery {
    pub error_kind: EncryptedContentError,
    pub encrypted_fields_removed: usize,
    pub null_content_fields_removed: usize,
    pub empty_reasoning_items_removed: usize,
    pub encrypted_tool_parts_replaced: usize,
    pub tool_outputs_changed: usize,
}

#[derive(Debug, Clone, Copy, Default, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EncryptedContentError {
    #[default]
    Reasoning,
    ToolOutput,
}

pub type ByteStream = Pin<Box<dyn Stream<Item = Result<Bytes>> + Send>>;

pub struct UpstreamResponse {
    pub status: u16,
    pub headers: HeaderMap,
    pub bytes: ByteStream,
}

#[derive(Default)]
pub struct Observation {
    /// Control-only event that can be held before exposing a response identity.
    pub recovery_preamble: bool,
    /// An explicit encrypted-content rejection without output or reported work.
    pub encrypted_rejection: Option<EncryptedContentError>,
    pub response_model: Option<String>,
    pub compaction_output: Option<bool>,
    pub usage: Option<Usage>,
    pub quotas: Vec<QuotaWindow>,
    pub terminal: bool,
    pub error: Option<Error>,
    pub disable_reason: Option<DisableReason>,
    pub content: bool,
}

pub struct GatewayEvent {
    pub document: ProtocolDocument,
    pub observation: Observation,
}

pub trait IngressAdapter: Send + Sync {
    fn parse(&self, headers: &HeaderMap, body: Value) -> Result<GatewayRequest>;
    fn encode(&self, document: &ProtocolDocument) -> Result<Bytes>;
    fn heartbeat(&self, request_id: Uuid) -> Bytes;
    fn failure(&self, request_id: Uuid, error: &Error) -> Bytes;
    fn unary_response(&self, document: &ProtocolDocument) -> Option<Value>;
}

pub trait ProviderDecoder: Send {
    fn push(
        &mut self,
        bytes: &[u8],
        ids: &mut IdentityMap,
        max_event_bytes: usize,
    ) -> Result<Vec<GatewayEvent>>;
    fn finish(&mut self) -> Result<()>;
    fn buffered_bytes(&self) -> usize;
}

pub trait ProviderAdapter: Send + Sync {
    fn validate(&self, request: &GatewayRequest, model: &ModelSpec) -> Result<()>;
    fn prepare(
        &self,
        request: &GatewayRequest,
        model: &ModelSpec,
        account: &Account,
        credentials: &Credentials,
        ids: &mut IdentityMap,
    ) -> Result<PreparedRequest>;
    fn decoder(&self) -> Box<dyn ProviderDecoder>;
    fn search_response(&self, body: &[u8]) -> Result<Usage>;
    fn compact_response(&self, body: &[u8]) -> Result<Observation>;
    fn headers(&self, headers: &HeaderMap) -> Observation;
    fn http_error(&self, status: u16, body: &[u8]) -> (Error, Option<DisableReason>);
    fn recover_encrypted_reasoning(
        &self,
        kind: RequestKind,
        request: &mut PreparedRequest,
        status: u16,
        error_body: &[u8],
    ) -> Result<Option<EncryptedContentRecovery>>;
    fn has_recoverable_encrypted_input(&self, request: &PreparedRequest) -> bool;
    fn recover_encrypted_stream(
        &self,
        request: &mut PreparedRequest,
        error: EncryptedContentError,
    ) -> Result<Option<EncryptedContentRecovery>>;
    fn refresh_request(
        &self,
        account: &Account,
        credentials: &Credentials,
    ) -> Result<PreparedRequest>;
    fn refreshed_credentials(
        &self,
        status: u16,
        body: &[u8],
        previous: &Credentials,
        account: &Account,
    ) -> Result<Credentials>;
    fn quota_request(
        &self,
        account: &Account,
        credentials: &Credentials,
    ) -> Result<PreparedRequest>;
    fn quota_response(&self, body: &[u8]) -> Result<Vec<QuotaWindow>>;
    fn reset_credits_request(
        &self,
        account: &Account,
        credentials: &Credentials,
    ) -> Result<PreparedRequest>;
    fn reset_credits_response(&self, body: &[u8]) -> Result<crate::resets::ResetCredits>;
    fn consume_reset_request(
        &self,
        account: &Account,
        credentials: &Credentials,
        operation: &crate::resets::ResetOperation,
    ) -> Result<PreparedRequest>;
    fn consume_reset_response(&self, body: &[u8]) -> Result<crate::resets::ResetResult>;
    fn models_request(
        &self,
        account: &Account,
        credentials: &Credentials,
    ) -> Result<PreparedRequest>;
    fn models_response(&self, body: &[u8]) -> Result<Vec<crate::providers::DiscoveredModel>>;
}

#[async_trait]
pub trait UpstreamTransport: Send + Sync {
    async fn send_once(
        &self,
        request: PreparedRequest,
        cancel: CancellationToken,
    ) -> Result<UpstreamResponse>;
}
