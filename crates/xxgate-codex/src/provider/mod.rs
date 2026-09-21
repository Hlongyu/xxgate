mod events;
mod models;
pub mod oauth;
pub mod oauth_browser;
pub mod oauth_response;
mod quota;
mod recovery;
mod references;
pub(crate) mod request;
mod resets;

use http::HeaderMap;
use xxgate_core::{
    Error, Result,
    accounts::{Account, Credentials, DisableReason},
    identity::IdentityMap,
    protocol::{GatewayRequest, Observation, PreparedRequest, ProviderAdapter, ProviderDecoder},
    providers::ModelSpec,
    quota::QuotaWindow,
};

pub struct CodexProvider;
impl ProviderAdapter for CodexProvider {
    fn has_recoverable_encrypted_input(&self, request: &PreparedRequest) -> bool {
        recovery::has_recoverable_input(request)
    }
    fn recover_encrypted_stream(
        &self,
        request: &mut xxgate_core::protocol::PreparedRequest,
        error: xxgate_core::protocol::EncryptedContentError,
    ) -> Result<Option<xxgate_core::protocol::EncryptedContentRecovery>> {
        recovery::clean(request, error)
    }
    fn recover_encrypted_reasoning(
        &self,
        kind: xxgate_core::protocol::RequestKind,
        request: &mut PreparedRequest,
        status: u16,
        error_body: &[u8],
    ) -> Result<Option<xxgate_core::protocol::EncryptedContentRecovery>> {
        recovery::prepare(kind, request, status, error_body)
    }
    fn compact_response(&self, body: &[u8]) -> Result<Observation> {
        events::compact_response(body)
    }
    fn search_response(&self, body: &[u8]) -> Result<xxgate_core::usage::Usage> {
        let value: serde_json::Value = serde_json::from_slice(body).map_err(|_| {
            Error::new(
                502,
                "invalid_search_response",
                "Search returned invalid JSON",
            )
        })?;
        if value
            .get("output")
            .and_then(serde_json::Value::as_str)
            .is_none()
        {
            return Err(Error::new(
                502,
                "invalid_search_response",
                "Search returned no output field",
            ));
        }
        Ok(xxgate_core::usage::Usage {
            search_calls: 1,
            source: "search_response".into(),
            complete: true,
            ..Default::default()
        })
    }
    fn reset_credits_request(&self, a: &Account, c: &Credentials) -> Result<PreparedRequest> {
        resets::request(a, c, None)
    }
    fn reset_credits_response(&self, body: &[u8]) -> Result<xxgate_core::resets::ResetCredits> {
        resets::credits(body)
    }
    fn consume_reset_request(
        &self,
        a: &Account,
        c: &Credentials,
        operation: &xxgate_core::resets::ResetOperation,
    ) -> Result<PreparedRequest> {
        resets::request(a, c, Some(operation))
    }
    fn consume_reset_response(&self, body: &[u8]) -> Result<xxgate_core::resets::ResetResult> {
        resets::result(body)
    }
    fn models_request(
        &self,
        account: &Account,
        credentials: &Credentials,
    ) -> Result<PreparedRequest> {
        models::request(account, credentials)
    }
    fn models_response(&self, body: &[u8]) -> Result<Vec<xxgate_core::providers::DiscoveredModel>> {
        models::response(body)
    }
    fn validate(&self, request: &GatewayRequest, model: &ModelSpec) -> Result<()> {
        request::validate(request, model)
    }
    fn prepare(
        &self,
        r: &GatewayRequest,
        m: &ModelSpec,
        a: &Account,
        c: &Credentials,
        ids: &mut IdentityMap,
    ) -> Result<PreparedRequest> {
        if r.kind == xxgate_core::protocol::RequestKind::Search {
            crate::search::prepare(r, m, a, c, ids)
        } else {
            request::prepare(r, m, a, c, ids)
        }
    }
    fn decoder(&self) -> Box<dyn ProviderDecoder> {
        Box::new(events::CodexDecoder::default())
    }
    fn headers(&self, headers: &HeaderMap) -> Observation {
        Observation {
            quotas: quota::headers(headers),
            ..Observation::default()
        }
    }
    fn http_error(&self, status: u16, body: &[u8]) -> (Error, Option<DisableReason>) {
        classify_error(status, body)
    }
    fn refresh_request(
        &self,
        account: &Account,
        credentials: &Credentials,
    ) -> Result<PreparedRequest> {
        oauth::refresh_request(account, credentials)
    }
    fn refreshed_credentials(
        &self,
        status: u16,
        body: &[u8],
        previous: &Credentials,
        account: &Account,
    ) -> Result<Credentials> {
        oauth::refreshed(status, body, previous, account)
    }
    fn quota_request(
        &self,
        account: &Account,
        credentials: &Credentials,
    ) -> Result<PreparedRequest> {
        quota::request(account, credentials)
    }
    fn quota_response(&self, body: &[u8]) -> Result<Vec<QuotaWindow>> {
        quota::response(body)
    }
}

pub(crate) fn classify_error(status: u16, body: &[u8]) -> (Error, Option<DisableReason>) {
    let parsed = serde_json::from_slice::<serde_json::Value>(body).ok();
    let (error, disable) = classify_parsed_error(status, parsed.as_ref());
    (error.with_upstream(error_facts(parsed.as_ref())), disable)
}

fn classify_parsed_error(
    status: u16,
    parsed: Option<&serde_json::Value>,
) -> (Error, Option<DisableReason>) {
    let code = parsed
        .as_ref()
        .and_then(|v| {
            v.pointer("/error/code")
                .or_else(|| v.pointer("/error/type"))
                .or_else(|| v.get("code"))
        })
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    if status == 401
        || [
            "invalid_grant",
            "token_revoked",
            "account_deactivated",
            "invalid_token",
        ]
        .contains(&code)
    {
        return (
            Error::new(
                502,
                "oauth_invalid",
                "The upstream account authorization is invalid",
            ),
            Some(DisableReason::OauthInvalid),
        );
    }
    if [
        "insufficient_quota",
        "usage_limit_reached",
        "quota_exhausted",
    ]
    .contains(&code)
    {
        return (
            Error::new(
                429,
                "upstream_quota_exhausted",
                "The upstream account quota is exhausted",
            ),
            Some(DisableReason::QuotaExhausted),
        );
    }
    let (code, message) = match status {
        429 => (
            "upstream_rate_limited",
            "The upstream temporarily rate-limited this request",
        ),
        400 | 422 => (
            "upstream_invalid_request",
            "The upstream rejected the request parameters or context",
        ),
        403 => (
            "upstream_forbidden",
            "The upstream account cannot access this capability",
        ),
        404 => (
            "upstream_not_found",
            "The upstream model or endpoint was not found",
        ),
        _ => (
            "upstream_error",
            "The upstream could not complete this request",
        ),
    };
    (
        Error::new(
            if (400..500).contains(&status) {
                status
            } else {
                502
            },
            code,
            message,
        )
        .with_client_message(parsed.as_ref().and_then(|v| {
            v.pointer("/error/message")
                .and_then(serde_json::Value::as_str)
                .or_else(|| v.get("detail").and_then(serde_json::Value::as_str))
                .or_else(|| v.get("message").and_then(serde_json::Value::as_str))
        })),
        None,
    )
}

fn error_facts(value: Option<&serde_json::Value>) -> Option<xxgate_core::types::UpstreamError> {
    let value = value?;
    let code = value
        .pointer("/error/code")
        .or_else(|| value.pointer("/error/type"))
        .or_else(|| value.get("code"))
        .and_then(serde_json::Value::as_str)
        .filter(|code| {
            [
                "unsupported_parameter",
                "unknown_parameter",
                "invalid_value",
                "invalid_request_error",
                "invalid_encrypted_content",
                "context_length_exceeded",
                "rate_limit_exceeded",
                "insufficient_quota",
                "usage_limit_reached",
                "invalid_token",
                "token_revoked",
                "account_deactivated",
                "server_error",
            ]
            .contains(code)
        });
    let message = value
        .pointer("/error/message")
        .or_else(|| value.get("detail"))
        .or_else(|| value.get("message"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    let unsupported = message
        .strip_prefix("Unsupported parameter: ")
        .or_else(|| message.strip_prefix("Unknown parameter: "));
    let param = value
        .pointer("/error/param")
        .and_then(serde_json::Value::as_str)
        .or(unsupported)
        .map(|p| p.trim().trim_matches(['\'', '"', '.']))
        .filter(|param| {
            [
                "model",
                "input",
                "instructions",
                "tools",
                "tool_choice",
                "parallel_tool_calls",
                "max_output_tokens",
                "max_completion_tokens",
                "temperature",
                "top_p",
                "stream_options",
                "service_tier",
                "reasoning",
                "text",
                "store",
                "stream",
                "previous_response_id",
                "background",
                "client_metadata",
                "prompt_cache_key",
                "context_management",
                "truncation",
            ]
            .contains(param)
        });
    let reason = if message == "Our servers are currently overloaded. Please try again later." {
        Some("upstream_overloaded")
    } else if code == Some("invalid_encrypted_content")
        && message == "Encrypted function output content could not be decrypted or decoded."
    {
        Some("invalid_encrypted_tool_output")
    } else if unsupported.is_some()
        || matches!(code, Some("unsupported_parameter" | "unknown_parameter"))
    {
        Some("unsupported_parameter")
    } else {
        match code {
            Some("invalid_encrypted_content") => Some("invalid_encrypted_content"),
            Some("context_length_exceeded") => Some("context_length_exceeded"),
            Some("rate_limit_exceeded") => Some("rate_limit_exceeded"),
            _ => None,
        }
    };
    if code.is_none() && reason.is_none() && param.is_none() {
        return None;
    }
    Some(xxgate_core::types::UpstreamError {
        code: code.map(str::to_owned),
        reason: reason.map(str::to_owned),
        param: param.map(str::to_owned),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn upstream_facts_preserve_known_causes_without_free_text() {
        let (error, _) = classify_error(
            400,
            br#"{"detail":"Unsupported parameter: max_output_tokens"}"#,
        );
        let facts = error.upstream.unwrap();
        assert_eq!(facts.reason.as_deref(), Some("unsupported_parameter"));
        assert_eq!(facts.param.as_deref(), Some("max_output_tokens"));
        let (error,_) = classify_error(400, br#"{"error":{"code":"invalid_encrypted_content","param":"input","message":"PRIVATE_REQUEST_CONTENT"}}"#);
        assert_eq!(
            error.upstream.as_ref().unwrap().reason.as_deref(),
            Some("invalid_encrypted_content")
        );
        assert_eq!(error.client_message(), "PRIVATE_REQUEST_CONTENT");
        assert!(!serde_json::to_string(&error).unwrap().contains("PRIVATE_"));
        let (error,_) = classify_error(400, br#"{"error":{"code":"PRIVATE_CODE","param":"PRIVATE_PARAM","message":"PRIVATE_TEXT"}}"#);
        assert!(error.upstream.is_none());
    }

    #[test]
    fn upstream_error_detail_is_returned_only_to_the_requesting_client() {
        for body in [
            json!({"error":{"code":"invalid_encrypted_content","message":"PRIVATE_HISTORY: encrypted_content is invalid"}}),
            json!({"detail":"PRIVATE_HISTORY: encrypted_content is invalid"}),
            json!({"message":"PRIVATE_HISTORY: encrypted_content is invalid"}),
        ] {
            let (error, disable) = classify_error(400, &serde_json::to_vec(&body).unwrap());
            assert_eq!(error.status, 400);
            assert_eq!(error.code, "upstream_invalid_request");
            assert_eq!(
                error.client_message(),
                "PRIVATE_HISTORY: encrypted_content is invalid"
            );
            assert!(disable.is_none());
            assert!(!error.message.contains("PRIVATE_"));
            assert!(!error.to_string().contains("PRIVATE_"));
            assert!(!format!("{error:?}").contains("PRIVATE_"));
            let saved = serde_json::to_string(&error).unwrap();
            assert!(!saved.contains("PRIVATE_"));
            let restored: Error = serde_json::from_str(&saved).unwrap();
            assert_eq!(restored.client_message(), restored.message);
        }
        let (error, _) = classify_error(502, b"<html>PRIVATE_PROXY_RESPONSE</html>");
        assert_eq!(error.client_message(), error.message);
    }
}
