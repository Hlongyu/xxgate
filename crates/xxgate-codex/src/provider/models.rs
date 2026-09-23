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
    request_with_version(account, credentials, &crate::model_version::current())
}

fn request_with_version(
    account: &Account,
    credentials: &Credentials,
    version: &str,
) -> Result<PreparedRequest> {
    let mut account = account.clone();
    account.profile.user_agent = account.profile.user_agent.replace(
        &format!("codex_cli_rs/{}", account.profile.codex_version),
        &format!("codex_cli_rs/{version}"),
    );
    account.profile.codex_version = version.into();
    let mut headers = super::request::auth_headers(&account, credentials)?;
    headers.insert(
        "version",
        http::HeaderValue::from_str(version)
            .map_err(|_| Error::invalid("Invalid models client version"))?,
    );
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
        headers,
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
                raw: Some(model.clone()),
            },
        );
    }
    Ok(result.into_values().collect())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn discovery_version_updates_query_and_headers_without_changing_account() {
        let account: Account = serde_json::from_value(serde_json::json!({
            "id": uuid::Uuid::new_v4(), "name":"test", "provider":"openai",
            "access_kind":"codex_oauth", "enabled":true, "disable_reason":null,
            "max_inflight":1, "upstream_account_id":"account-id",
            "upstream_base_url":"https://chatgpt.com/backend-api/codex",
            "models":[], "profile":xxgate_core::accounts::ClientProfile::default(),
            "version":1,"credential_version":1,"credential_expires_at":null,
            "created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-01T00:00:00Z"
        }))
        .unwrap();
        let credentials: Credentials =
            serde_json::from_value(serde_json::json!({"access_token":"test","expires_at":null}))
                .unwrap();
        let request = request_with_version(&account, &credentials, "0.155.1").unwrap();
        assert!(request.url.ends_with("/models?client_version=0.155.1"));
        assert_eq!(request.headers["version"], "0.155.1");
        assert!(
            request.headers["user-agent"]
                .to_str()
                .unwrap()
                .starts_with("codex_cli_rs/0.155.1 ")
        );
        assert_eq!(request.headers["chatgpt-account-id"], "account-id");
        assert_eq!(account.profile.codex_version, "0.153.4");
    }
    #[test]
    fn preserves_complete_upstream_capabilities() {
        let models=response(br#"{"models":[{"slug":"gpt-test","display_name":"Test","context_window":200000,"description":"PRIVATE_TEXT","base_instructions":"PRIVATE_PROMPT"},{"slug":"gpt-test","display_name":"Test","context_window":200000}]}"#).unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].context_window, Some(200000));
        let original = serde_json::json!({"slug":"test","display_name":"Test","base_instructions":"instructions","unknown":{"nested":[1,null,true]},"service_tiers":[]});
        let decoded = response(
            serde_json::json!({"models":[original.clone()]})
                .to_string()
                .as_bytes(),
        )
        .unwrap();
        assert_eq!(decoded[0].raw.as_ref(), Some(&original));
        assert!(response(br#"{"error":"denied"}"#).is_err());
        assert!(response(br#"{"models":[{}]}"#).is_err());
        assert!(response(br#"{"models":[]}"#).unwrap().is_empty());
    }
}
