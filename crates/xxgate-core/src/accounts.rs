use crate::{Error, Result, types::ModelRef};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DisableReason {
    AdminDisabled,
    Quota5hExhausted,
    Quota7dExhausted,
    QuotaExhausted,
    OauthInvalid,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientProfile {
    pub installation_id: Uuid,
    pub codex_version: String,
    pub user_agent: String,
    pub tls_backend: String,
}

impl Default for ClientProfile {
    fn default() -> Self {
        let os = os_info::get();
        Self {
            installation_id: Uuid::new_v4(),
            codex_version: "0.153.4".into(),
            user_agent: format!(
                "codex_cli_rs/0.153.4 ({} {}; {}) unknown",
                os.os_type(),
                os.version(),
                os.architecture().unwrap_or("unknown")
            ),
            tls_backend: "native".into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Account {
    pub id: Uuid,
    #[serde(default = "crate::groups::default_group_ids")]
    pub group_ids: Vec<Uuid>,
    pub name: String,
    pub provider: String,
    pub access_kind: String,
    pub enabled: bool,
    #[serde(default)]
    pub codex_only: bool,
    pub disable_reason: Option<DisableReason>,
    pub max_inflight: u32,
    pub upstream_account_id: String,
    pub upstream_base_url: String,
    pub models: Vec<String>,
    #[serde(default)]
    pub models_restricted: bool,
    #[serde(default)]
    pub model_catalog: Option<crate::providers::AccountModelCatalog>,
    pub profile: ClientProfile,
    pub version: i64,
    pub credential_version: i64,
    pub credential_expires_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl Account {
    pub fn accepts(&self, source: crate::clients::ClientSource) -> bool {
        !self.codex_only || source == crate::clients::ClientSource::Codex
    }
    pub fn supports(&self, model: &ModelRef) -> bool {
        self.provider == model.provider
            && self.access_kind == model.access_kind
            && ((!self.models_restricted && self.models.is_empty())
                || self.models.contains(&model.model))
            && self
                .model_catalog
                .as_ref()
                .filter(|c| c.synced_at.is_some())
                .is_none_or(|c| c.models.iter().any(|m| m.id == model.model))
    }
    pub fn validate(&self, allow_http: bool) -> Result<()> {
        if self.group_ids.len() > 64
            || self
                .group_ids
                .iter()
                .collect::<std::collections::HashSet<_>>()
                .len()
                != self.group_ids.len()
        {
            return Err(Error::invalid("Choose at most 64 distinct account groups"));
        }
        if self.models.len() > 512
            || self
                .models
                .iter()
                .any(|m| m.trim().is_empty() || m.len() > 160)
        {
            return Err(Error::invalid("Invalid account model selection"));
        }
        if self.name.trim().is_empty() || self.name.len() > 128 {
            return Err(Error::invalid("Account name must contain 1 to 128 bytes"));
        }
        if self.max_inflight == 0 || self.max_inflight > 1000 {
            return Err(Error::invalid(
                "Account concurrency must be between 1 and 1000",
            ));
        }
        if self.upstream_account_id.trim().is_empty() || self.upstream_account_id.len() > 256 {
            return Err(Error::invalid("An upstream account ID is required"));
        }
        if self.provider != "openai" || self.access_kind != "codex_oauth" {
            return Err(Error::invalid(
                "Only the Codex OAuth provider is currently supported",
            ));
        }
        let url = url::Url::parse(&self.upstream_base_url)
            .map_err(|_| Error::invalid("Invalid upstream URL"))?;
        if url.host_str().is_none()
            || url.username() != ""
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
            || !(url.scheme() == "https" || allow_http && url.scheme() == "http")
        {
            return Err(Error::invalid(
                "Upstream URL requires HTTPS and must not include credentials, query or fragment",
            ));
        }
        if !["native", "rustls"].contains(&self.profile.tls_backend.as_str()) {
            return Err(Error::invalid("Unsupported TLS backend"));
        }
        if self.profile.codex_version != "0.153.4" {
            return Err(Error::invalid("The supported Codex baseline is 0.153.4"));
        }
        http::HeaderValue::from_str(&self.profile.user_agent)
            .map_err(|_| Error::invalid("Invalid User-Agent"))?;
        Ok(())
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Credentials {
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: String,
    #[serde(default)]
    pub id_token: String,
    pub expires_at: Option<DateTime<Utc>>,
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Credentials([redacted])")
    }
}

impl Credentials {
    pub fn validate(&self) -> Result<()> {
        if self.access_token.is_empty()
            || self.access_token.len() > 32768
            || self.refresh_token.len() > 32768
            || self.id_token.len() > 32768
        {
            return Err(Error::invalid("Invalid credential length"));
        }
        http::HeaderValue::from_str(&format!("Bearer {}", self.access_token))
            .map_err(|_| Error::invalid("Invalid access token"))?;
        Ok(())
    }
}
