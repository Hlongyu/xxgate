use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use bytes::Bytes;
use chrono::{DateTime, Utc};
use http::{HeaderMap, HeaderValue, Method};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use xxgate_core::{
    Error, Result,
    accounts::{Account, Credentials},
    protocol::PreparedRequest,
};

pub const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
pub const ISSUER: &str = "https://auth.openai.com";

fn control(url: String, body: Value) -> Result<PreparedRequest> {
    let mut headers = HeaderMap::new();
    headers.insert("content-type", HeaderValue::from_static("application/json"));
    Ok(PreparedRequest {
        method: Method::POST,
        url,
        headers,
        body: Bytes::from(
            serde_json::to_vec(&body).map_err(|_| Error::invalid("Invalid OAuth payload"))?,
        ),
        account_id: None,
        profile_version: 1,
        tls_backend: "native".into(),
    })
}

pub(crate) fn refresh_request(a: &Account, c: &Credentials) -> Result<PreparedRequest> {
    let mut request = control(
        format!("{ISSUER}/oauth/token"),
        json!({"client_id":CLIENT_ID,"grant_type":"refresh_token","refresh_token":c.refresh_token}),
    )?;
    request.account_id = Some(a.id);
    request.profile_version = a.version;
    Ok(request)
}

pub(crate) fn refreshed(
    status: u16,
    body: &[u8],
    previous: &Credentials,
    account: &Account,
) -> Result<Credentials> {
    let value: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
    if !(200..300).contains(&status) {
        let code = value
            .pointer("/error/code")
            .or_else(|| value.get("error"))
            .or_else(|| value.get("code"))
            .and_then(Value::as_str)
            .unwrap_or("");
        if status == 401
            || [
                "invalid_grant",
                "refresh_token_expired",
                "refresh_token_reused",
                "refresh_token_invalidated",
            ]
            .contains(&code)
        {
            return Err(Error::new(
                401,
                "oauth_invalid",
                "OAuth refresh authorization is no longer valid",
            ));
        }
        return Err(Error::new(
            502,
            "oauth_refresh_failed",
            "OAuth refresh failed temporarily",
        ));
    }
    let credentials = tokens(value, Some(previous))?;
    let details = account_details(&credentials)?;
    if details.account_id != account.upstream_account_id {
        return Err(Error::new(
            401,
            "oauth_invalid",
            "Refreshed credentials belong to a different upstream account",
        ));
    }
    Ok(credentials)
}

pub fn tokens(value: Value, previous: Option<&Credentials>) -> Result<Credentials> {
    let access_token = value
        .get("access_token")
        .and_then(Value::as_str)
        .filter(|v| !v.is_empty())
        .ok_or_else(|| {
            Error::new(
                502,
                "invalid_oauth_response",
                "OAuth response omitted access_token",
            )
        })?
        .to_owned();
    let expires_at = value
        .get("expires_in")
        .and_then(Value::as_i64)
        .filter(|n| *n > 0 && *n < 31_536_000)
        .map(|n| Utc::now() + chrono::Duration::seconds(n))
        .or_else(|| {
            jwt(&access_token)
                .and_then(|v| v.get("exp").and_then(Value::as_i64))
                .and_then(|n| DateTime::from_timestamp(n, 0))
        });
    Ok(Credentials {
        access_token,
        refresh_token: value
            .get("refresh_token")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or_else(|| previous.map(|v| v.refresh_token.clone()))
            .unwrap_or_default(),
        id_token: value
            .get("id_token")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or_else(|| previous.map(|v| v.id_token.clone()))
            .unwrap_or_default(),
        expires_at,
    })
}
fn jwt(value: &str) -> Option<Value> {
    serde_json::from_slice(&URL_SAFE_NO_PAD.decode(value.split('.').nth(1)?).ok()?).ok()
}

#[derive(Debug, Clone, Serialize)]
pub struct AccountDetails {
    pub account_id: String,
    pub email: Option<String>,
    pub plan_type: Option<String>,
}
pub fn account_details(c: &Credentials) -> Result<AccountDetails> {
    // Claims are used as upstream metadata, never as authorization for the administrator API.
    let claims: Vec<_> = [&c.id_token, &c.access_token]
        .into_iter()
        .filter_map(|token| jwt(token))
        .collect();
    let account_id = claims
        .iter()
        .find_map(claim_account_id)
        .ok_or_else(|| Error::invalid("Credential claims do not contain a ChatGPT account ID"))?;
    let matching: Vec<_> = claims
        .iter()
        .filter(|v| claim_account_id(v) == Some(account_id))
        .collect();
    Ok(AccountDetails {
        account_id: account_id.into(),
        email: matching
            .iter()
            .find_map(|v| v.get("email").and_then(Value::as_str).map(str::to_owned)),
        // A refresh can replace the access token while retaining an older ID token.
        plan_type: matching
            .iter()
            .rev()
            .find_map(|v| {
                v.pointer("/https:~1~1api.openai.com~1auth/chatgpt_plan_type")
                    .or_else(|| v.get("chatgpt_plan_type"))
                    .and_then(Value::as_str)
            })
            .map(str::to_ascii_lowercase)
            .filter(|p| {
                matches!(
                    p.as_str(),
                    "free" | "plus" | "pro" | "go" | "team" | "business" | "enterprise" | "edu"
                )
            }),
    })
}

fn claim_account_id(value: &Value) -> Option<&str> {
    value
        .pointer("/https:~1~1api.openai.com~1auth/chatgpt_account_id")
        .or_else(|| value.get("chatgpt_account_id"))
        .and_then(Value::as_str)
        .filter(|v| !v.is_empty())
}

#[cfg(test)]
mod identity_tests {
    use super::*;

    fn token(value: Value) -> String {
        format!(
            "header.{}.signature",
            URL_SAFE_NO_PAD.encode(value.to_string())
        )
    }

    #[test]
    fn plan_claims_cover_subscriptions_without_guessing_missing_or_unknown_plans() {
        for (claim, expected) in [
            (json!("free"), Some("free")),
            (json!("plus"), Some("plus")),
            (json!("Pro"), Some("pro")),
            (Value::Null, None),
            (json!("<script>"), None),
            (json!("future_plan"), None),
        ] {
            let c = Credentials {
                id_token: token(
                    json!({"email":"a@example.com","https://api.openai.com/auth":{"chatgpt_account_id":"account-a","chatgpt_plan_type":claim}}),
                ),
                access_token: "opaque".into(),
                refresh_token: String::new(),
                expires_at: None,
            };
            let d = account_details(&c).unwrap();
            assert_eq!(d.plan_type.as_deref(), expected);
            assert_eq!(d.email.as_deref(), Some("a@example.com"));
        }
    }

    #[test]
    fn refreshed_plan_uses_matching_access_claims_and_preserves_identity_email() {
        let mut c = Credentials {
            id_token: token(
                json!({"chatgpt_account_id":"a","email":"a@example.com","chatgpt_plan_type":"free"}),
            ),
            access_token: token(json!({"chatgpt_account_id":"a","chatgpt_plan_type":"pro"})),
            refresh_token: String::new(),
            expires_at: None,
        };
        let d = account_details(&c).unwrap();
        assert_eq!(d.account_id, "a");
        assert_eq!(d.email.as_deref(), Some("a@example.com"));
        assert_eq!(d.plan_type.as_deref(), Some("pro"));
        c.access_token = token(json!({"chatgpt_account_id":"b","chatgpt_plan_type":"plus"}));
        assert_eq!(
            account_details(&c).unwrap().plan_type.as_deref(),
            Some("free")
        );
        c.access_token = token(json!({"chatgpt_account_id":"a","chatgpt_plan_type":"future_plan"}));
        assert_eq!(account_details(&c).unwrap().plan_type, None);
        c.id_token = "invalid".into();
        c.access_token = "opaque".into();
        assert!(account_details(&c).is_err());
    }
}

#[derive(Clone, Deserialize, Serialize)]
pub struct DeviceCode {
    pub device_auth_id: String,
    #[serde(alias = "usercode")]
    pub user_code: String,
    #[serde(default)]
    pub interval: Value,
}
impl std::fmt::Debug for DeviceCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("DeviceCode([redacted])")
    }
}
impl DeviceCode {
    pub fn poll_seconds(&self) -> u64 {
        self.interval
            .as_u64()
            .or_else(|| self.interval.as_str().and_then(|s| s.parse().ok()))
            .unwrap_or(5)
            .clamp(3, 60)
    }
}

pub fn device_start() -> Result<PreparedRequest> {
    control(
        format!("{ISSUER}/api/accounts/deviceauth/usercode"),
        json!({"client_id":CLIENT_ID}),
    )
}
pub fn device_poll(code: &DeviceCode) -> Result<PreparedRequest> {
    control(
        format!("{ISSUER}/api/accounts/deviceauth/token"),
        json!({"device_auth_id":code.device_auth_id,"user_code":code.user_code}),
    )
}
pub fn exchange_device(value: &Value) -> Result<PreparedRequest> {
    let code = value
        .get("authorization_code")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            Error::new(
                502,
                "invalid_oauth_response",
                "Device authorization omitted authorization code",
            )
        })?;
    let verifier = value
        .get("code_verifier")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            Error::new(
                502,
                "invalid_oauth_response",
                "Device authorization omitted PKCE verifier",
            )
        })?;
    let body = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("grant_type", "authorization_code")
        .append_pair("code", code)
        .append_pair("redirect_uri", &format!("{ISSUER}/deviceauth/callback"))
        .append_pair("client_id", CLIENT_ID)
        .append_pair("code_verifier", verifier)
        .finish();
    let mut request = control(format!("{ISSUER}/oauth/token"), Value::Null)?;
    request.body = Bytes::from(body);
    request.headers.insert(
        "content-type",
        HeaderValue::from_static("application/x-www-form-urlencoded"),
    );
    Ok(request)
}
