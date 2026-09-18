use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Usage {
    #[serde(default)]
    pub search_calls: u64,
    pub input_tokens: Option<u64>,
    pub cached_input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub reasoning_output_tokens: Option<u64>,
    pub image_input_tokens: Option<u64>,
    pub image_output_tokens: Option<u64>,
    pub image_count: u32,
    pub image_tool_usage_reported: bool,
    pub service_tier: Option<String>,
    pub source: String,
    pub complete: bool,
    #[serde(default)]
    pub raw_usage: serde_json::Value,
}
