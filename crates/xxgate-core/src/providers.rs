use crate::{Error, Result, types::ModelRef};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DiscoveredModel {
    pub id: String,
    pub display_name: String,
    pub context_window: Option<i64>,
    /// Complete upstream capability object. Older catalogs have no raw object
    /// until their next successful sync; do not fabricate missing capabilities.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AccountModelCatalog {
    pub models: Vec<DiscoveredModel>,
    pub synced_at: Option<DateTime<Utc>>,
    pub attempted_at: Option<DateTime<Utc>>,
    pub error: Option<Error>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Capabilities {
    pub image_input: bool,
    pub image_generation: bool,
    pub tools: bool,
    pub reasoning: bool,
    pub fast: bool,
}
impl Default for Capabilities {
    fn default() -> Self {
        Self {
            image_input: true,
            image_generation: true,
            tools: true,
            reasoning: true,
            fast: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelSpec {
    pub id: String,
    pub upstream: ModelRef,
    pub enabled: bool,
    pub capabilities: Capabilities,
    pub version: i64,
}

impl ModelSpec {
    pub fn validate(&self) -> Result<()> {
        if self.id.is_empty()
            || self.id.len() > 160
            || self.upstream.model.is_empty()
            || self.upstream.model.len() > 160
        {
            return Err(Error::invalid("Invalid model name"));
        }
        if self.upstream.provider != "openai" || self.upstream.access_kind != "codex_oauth" {
            return Err(Error::invalid("Only Codex OAuth is implemented"));
        }
        Ok(())
    }
}
