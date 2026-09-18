use serde::{Deserialize, Serialize};

/// Inbound software classification, independent of authentication and transport.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClientSource {
    Codex,
    #[default]
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientOrigin {
    pub source: ClientSource,
    /// Field locations only: never prompt content or credential values.
    pub evidence: Vec<String>,
    pub rule: String,
    pub version: u32,
}

impl Default for ClientOrigin {
    fn default() -> Self {
        Self {
            source: ClientSource::Unknown,
            evidence: vec![],
            rule: "unrecognized".into(),
            version: 1,
        }
    }
}
