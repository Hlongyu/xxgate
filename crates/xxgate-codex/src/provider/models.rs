use bytes::Bytes;
use http::Method;
use serde_json::Value;
use std::collections::BTreeMap;
use xxgate_core::{
    Error, Result,
    accounts::{Account, Credentials},
    protocol::PreparedRequest,
    providers::DiscoveredModel,
};

pub fn request(account: &Account, credentials: &Credentials) -> Result<PreparedRequest> {
    let mut url = url::Url::parse(&format!(
        "{}/models",
        account.upstream_base_url.trim_end_matches('/')
    ))
    .map_err(|_| Error::invalid("Invalid models URL"))?;
    url.query_pairs_mut()
        .append_pair("client_version", &account.profile.codex_version);
    Ok(PreparedRequest {
        method: Method::GET,
        url: url.into(),
        headers: super::request::auth_headers(account, credentials)?,
        body: Bytes::new(),
        account_id: Some(account.id),
        profile_version: account.version,
        tls_backend: account.profile.tls_backend.clone(),
    })
}
pub fn response(body: &[u8]) -> Result<Vec<DiscoveredModel>> {
    let value: Value = serde_json::from_slice(body).map_err(|_| {
        Error::new(
            502,
            "invalid_models_response",
            "上游模型目录不是有效的 JSON。",
        )
    })?;
    let models = value
        .get("models")
        .and_then(Value::as_array)
        .ok_or_else(|| Error::new(502, "invalid_models_response", "上游响应缺少 models 列表。"))?;
    if models.len() > 2000 {
        return Err(Error::new(
            502,
            "models_catalog_too_large",
            "上游模型目录超过大小限制。",
        ));
    }
    let mut result = BTreeMap::new();
    for model in models {
        let id = model
            .get("slug")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty() && s.len() <= 160 && !s.chars().any(char::is_control))
            .ok_or_else(|| {
                Error::new(
                    502,
                    "invalid_models_response",
                    "上游模型目录包含无效的模型名称。",
                )
            })?;
        let display_name = model
            .get("display_name")
            .and_then(Value::as_str)
            .filter(|s| s.len() <= 256 && !s.chars().any(char::is_control))
            .unwrap_or(id)
            .to_owned();
        let context_window = model
            .get("context_window")
            .and_then(Value::as_i64)
            .filter(|n| *n > 0);
        result.insert(
            id.to_owned(),
            DiscoveredModel {
                id: id.into(),
                display_name,
                context_window,
            },
        );
    }
    Ok(result.into_values().collect())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn decodes_codex_catalog_without_persisting_prompts_or_descriptions() {
        let models=response(br#"{"models":[{"slug":"gpt-test","display_name":"Test","context_window":200000,"description":"PRIVATE_TEXT","base_instructions":"PRIVATE_PROMPT"},{"slug":"gpt-test","display_name":"Test","context_window":200000}]}"#).unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].context_window, Some(200000));
        assert!(!serde_json::to_string(&models).unwrap().contains("PRIVATE"));
        assert!(response(br#"{"error":"denied"}"#).is_err());
        assert!(response(br#"{"models":[{}]}"#).is_err());
        assert!(response(br#"{"models":[]}"#).unwrap().is_empty());
    }
}
