use http::HeaderMap;
use serde::Serialize;
use serde_json::Value;
use xxgate_core::Error;

/// Only allowlisted diagnostics can leave the OAuth response decoder. Never log
/// the response body: successful responses contain authorization codes or tokens.
#[derive(Clone, Debug, Serialize)]
pub struct OAuthDiagnostics {
    pub upstream_status: u16,
    pub upstream_error_code: Option<&'static str>,
    pub upstream_request_id: Option<String>,
    pub cf_ray: Option<String>,
    pub content_kind: &'static str,
    pub challenge: bool,
}

pub struct OAuthResponse {
    pub body: Value,
    pub diagnostics: OAuthDiagnostics,
}

impl OAuthResponse {
    pub fn decode(status: u16, headers: &HeaderMap, bytes: &[u8]) -> Self {
        let body: Value = serde_json::from_slice(bytes).unwrap_or(Value::Null);
        let raw_code = body
            .pointer("/error/code")
            .or_else(|| body.pointer("/error/type"))
            .or_else(|| body.get("error").filter(|v| v.is_string()))
            .or_else(|| body.get("code"))
            .and_then(Value::as_str);
        let known = [
            "unsupported_country_region_territory",
            "device_authentication_disabled",
            "device_auth_disabled",
            "authorization_pending",
            "slow_down",
            "access_denied",
            "expired_token",
            "invalid_client",
            "invalid_grant",
            "rate_limit_exceeded",
        ];
        let upstream_error_code = raw_code.map(|code| {
            known
                .into_iter()
                .find(|known| *known == code)
                .unwrap_or("unrecognized")
        });
        let header = |name: &str| {
            headers
                .get(name)
                .and_then(|h| h.to_str().ok())
                .filter(|s| {
                    !s.is_empty()
                        && s.len() <= 256
                        && s.bytes()
                            .all(|b| b.is_ascii_alphanumeric() || b"-_.:".contains(&b))
                })
                .map(str::to_owned)
        };
        let json = body.is_object() || body.is_array();
        let html = headers
            .get("content-type")
            .and_then(|h| h.to_str().ok())
            .is_some_and(|s| s.starts_with("text/html"));
        Self {
            body,
            diagnostics: OAuthDiagnostics {
                upstream_status: status,
                upstream_error_code,
                upstream_request_id: header("x-request-id"),
                cf_ray: header("cf-ray"),
                content_kind: if json {
                    "json"
                } else if html {
                    "html"
                } else {
                    "other"
                },
                challenge: headers.get("cf-mitigated").and_then(|h| h.to_str().ok())
                    == Some("challenge"),
            },
        }
    }
    pub fn status(&self) -> u16 {
        self.diagnostics.upstream_status
    }
    pub fn pending(&self) -> bool {
        if self.diagnostics.challenge || self.diagnostics.content_kind != "json" {
            return false;
        }
        matches!(self.status(), 403 | 404)
            && matches!(
                self.diagnostics.upstream_error_code,
                None | Some("authorization_pending")
            )
    }
    pub fn error(&self) -> Error {
        let d = &self.diagnostics;
        let (code, message) = if d.upstream_error_code
            == Some("unsupported_country_region_territory")
        {
            (
                "oauth_region_unsupported",
                "OpenAI 拒绝了当前网络出口所在地区的授权请求（unsupported_country_region_territory）。这不是账户未开启设备授权。",
            )
        } else if d.challenge || d.content_kind == "html" && d.upstream_status == 403 {
            (
                "oauth_network_challenge",
                "OpenAI 授权接口返回了网络安全验证页面，网关无法继续设备授权。",
            )
        } else if matches!(
            d.upstream_error_code,
            Some("device_authentication_disabled" | "device_auth_disabled")
        ) {
            (
                "oauth_device_disabled",
                "OpenAI 明确报告设备授权未开启，请检查账户或工作区的设备授权设置。",
            )
        } else if d.upstream_status == 429 {
            (
                "oauth_rate_limited",
                "OpenAI 授权接口暂时限流，请稍后再试。",
            )
        } else if d.upstream_status == 200 {
            (
                "invalid_oauth_response",
                "OpenAI 授权接口返回了无法识别的响应。",
            )
        } else {
            (
                "oauth_upstream_rejected",
                "OpenAI 授权接口拒绝了请求。可在操作审计中查看脱敏诊断信息。",
            )
        };
        Error::new(
            502,
            code,
            format!("{message}（HTTP {}）", d.upstream_status),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn region_rejection_is_not_reported_as_disabled_device_auth() {
        let response=OAuthResponse::decode(403,&HeaderMap::new(),br#"{"error":{"code":"unsupported_country_region_territory","message":"PRIVATE_DATA"}}"#);
        assert_eq!(response.error().code, "oauth_region_unsupported");
        assert!(!response.pending());
        assert!(
            !serde_json::to_string(&response.diagnostics)
                .unwrap()
                .contains("PRIVATE_DATA")
        );
    }
    #[test]
    fn pending_statuses_do_not_hide_challenges_or_known_errors() {
        assert!(OAuthResponse::decode(403, &HeaderMap::new(), b"{}").pending());
        assert!(
            OAuthResponse::decode(
                404,
                &HeaderMap::new(),
                br#"{"error":"authorization_pending"}"#
            )
            .pending()
        );
        let headers = HeaderMap::from_iter([
            (
                "cf-mitigated".parse().unwrap(),
                "challenge".parse().unwrap(),
            ),
            (
                "content-type".parse().unwrap(),
                "text/html".parse().unwrap(),
            ),
        ]);
        let response = OAuthResponse::decode(403, &headers, b"<html>challenge</html>");
        assert!(!response.pending());
        assert_eq!(response.error().code, "oauth_network_challenge");
    }
    #[test]
    fn diagnostics_never_include_tokens_or_arbitrary_error_values() {
        for body in [
            json!({"access_token":"PRIVATE_TOKEN","device_auth_id":"PRIVATE_ID","user_code":"PRIVATE_CODE"}),
            json!({"error":{"code":"PRIVATE_TOKEN","message":"PRIVATE_PROMPT"}}),
        ] {
            let r =
                OAuthResponse::decode(400, &HeaderMap::new(), &serde_json::to_vec(&body).unwrap());
            assert!(
                !serde_json::to_string(&r.diagnostics)
                    .unwrap()
                    .contains("PRIVATE_")
            );
            assert!(!r.error().message.contains("PRIVATE_"));
        }
    }
}
