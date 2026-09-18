use serde::{Deserialize, Serialize};

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Clone, Serialize, Deserialize, thiserror::Error)]
#[error("{message}")]
pub struct Error {
    pub code: String,
    pub message: String,
    pub status: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream: Option<Box<UpstreamError>>,
    // Upstream text may quote request content. Return it only to this caller;
    // keep persistence, Display and Debug limited to the classified message.
    #[serde(skip)]
    client_message: Option<ClientMessage>,
}

/// Allowlisted facts extracted by the provider; never free-form upstream text.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct UpstreamError {
    pub code: Option<String>,
    pub reason: Option<String>,
    pub param: Option<String>,
}

#[derive(Clone)]
struct ClientMessage(String);

impl std::fmt::Debug for ClientMessage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("<transient client message>")
    }
}

impl Error {
    pub fn new(status: u16, code: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            status,
            upstream: None,
            client_message: None,
        }
    }
    pub fn with_upstream(mut self, facts: Option<UpstreamError>) -> Self {
        self.upstream = facts.map(Box::new);
        self
    }
    pub fn with_client_message(mut self, message: Option<&str>) -> Self {
        self.client_message = message
            .filter(|message| !message.trim().is_empty())
            .map(|message| ClientMessage(message.chars().take(4096).collect()));
        self
    }
    pub fn client_message(&self) -> &str {
        self.client_message.as_ref().map_or(&self.message, |m| &m.0)
    }
    pub fn invalid(message: impl Into<String>) -> Self {
        Self::new(400, "invalid_request", message)
    }
    pub fn storage() -> Self {
        Self::new(
            503,
            "storage_unavailable",
            "Persistent storage is unavailable",
        )
    }
    pub fn cancelled() -> Self {
        Self::new(499, "client_cancelled", "The client disconnected")
    }
    pub fn unauthorized() -> Self {
        Self::new(401, "unauthorized", "Authentication is required")
    }
    pub fn not_found() -> Self {
        Self::new(404, "not_found", "The requested resource was not found")
    }
    pub fn conflict() -> Self {
        Self::new(
            409,
            "version_conflict",
            "The resource changed; reload and try again",
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct ModelRef {
    pub provider: String,
    pub access_kind: String,
    pub model: String,
}

impl ModelRef {
    pub fn codex(model: impl Into<String>) -> Self {
        Self {
            provider: "openai".into(),
            access_kind: "codex_oauth".into(),
            model: model.into(),
        }
    }
}
