use super::oauth::{CLIENT_ID, ISSUER};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use bytes::Bytes;
use http::{HeaderMap, HeaderValue, Method};
use sha2::{Digest, Sha256};
use xxgate_core::{Error, Result, access::random_secret, protocol::PreparedRequest};

pub const REDIRECT_URI: &str = "http://localhost:1455/auth/callback";

/// The verifier stays in server memory. It is never serialized to the browser or audit.
pub struct BrowserAuthorization {
    state: String,
    verifier: String,
}
impl Default for BrowserAuthorization {
    fn default() -> Self {
        Self::new()
    }
}
impl BrowserAuthorization {
    pub fn new() -> Self {
        Self {
            state: random_secret(32),
            verifier: random_secret(64),
        }
    }
    pub fn authorization_url(&self) -> Result<String> {
        let mut url = url::Url::parse(&format!("{ISSUER}/oauth/authorize"))
            .map_err(|_| Error::invalid("Invalid OAuth issuer"))?;
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(self.verifier.as_bytes()));
        url.query_pairs_mut()
            .append_pair("response_type", "code")
            .append_pair("client_id", CLIENT_ID)
            .append_pair("redirect_uri", REDIRECT_URI)
            .append_pair(
                "scope",
                "openid profile email offline_access api.connectors.read api.connectors.invoke",
            )
            .append_pair("code_challenge", &challenge)
            .append_pair("code_challenge_method", "S256")
            .append_pair("id_token_add_organizations", "true")
            .append_pair("codex_cli_simplified_flow", "true")
            .append_pair("state", &self.state)
            .append_pair("originator", "codex_cli_rs");
        Ok(url.into())
    }
    pub fn callback_code(&self, callback: &str) -> Result<String> {
        let callback = callback.trim();
        if callback.len() > 16384 {
            return Err(Error::invalid(
                "回调链接过长，请粘贴浏览器地址栏中的完整链接。",
            ));
        }
        let url =
            url::Url::parse(callback).map_err(|_| Error::invalid("请粘贴完整的回调链接。"))?;
        let expected = url::Url::parse(REDIRECT_URI)
            .map_err(|_| Error::invalid("Invalid OAuth redirect URI"))?;
        if url.origin() != expected.origin()
            || url.path() != expected.path()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.fragment().is_some()
        {
            return Err(Error::invalid(
                "回调地址不匹配，应为 http://localhost:1455/auth/callback 开头的完整链接。",
            ));
        }
        let pairs = url.query_pairs().collect::<Vec<_>>();
        let unique = |key: &str| -> Result<Option<String>> {
            let values = pairs.iter().filter(|(k, _)| k == key).collect::<Vec<_>>();
            if values.len() > 1 {
                return Err(Error::invalid("回调链接包含重复的授权参数。"));
            }
            Ok(values.first().map(|(_, v)| v.to_string()))
        };
        let state = unique("state")?.ok_or_else(|| {
            Error::new(
                400,
                "oauth_state_missing",
                "回调链接缺少 state，请复制完整链接。",
            )
        })?;
        if state != self.state {
            return Err(Error::new(
                400,
                "oauth_state_mismatch",
                "此回调链接不属于本次授权，请使用当前生成的授权链接。",
            ));
        }
        if unique("error")?.is_some() {
            return Err(Error::new(
                400,
                "oauth_authorization_denied",
                "OpenAI 未完成授权，请重新生成链接并完成授权。",
            ));
        }
        unique("code")?
            .filter(|c| !c.is_empty() && c.len() <= 8192 && !c.chars().any(char::is_control))
            .ok_or_else(|| {
                Error::new(
                    400,
                    "oauth_code_missing",
                    "回调链接缺少有效的 code，请完成授权后复制完整链接。",
                )
            })
    }
    pub fn exchange_request(&self, code: &str) -> Result<PreparedRequest> {
        let body = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("grant_type", "authorization_code")
            .append_pair("code", code)
            .append_pair("redirect_uri", REDIRECT_URI)
            .append_pair("client_id", CLIENT_ID)
            .append_pair("code_verifier", &self.verifier)
            .finish();
        Ok(PreparedRequest {
            method: Method::POST,
            url: format!("{ISSUER}/oauth/token"),
            headers: HeaderMap::from_iter([(
                "content-type"
                    .parse()
                    .map_err(|_| Error::invalid("Invalid OAuth header"))?,
                HeaderValue::from_static("application/x-www-form-urlencoded"),
            )]),
            body: Bytes::from(body),
            account_id: None,
            profile_version: 1,
            tls_backend: "native".into(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pkce_uses_s256_and_verifier_is_not_in_authorization_url() {
        let auth = BrowserAuthorization {
            state: "test-state".into(),
            verifier: "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk".into(),
        };
        let link = auth.authorization_url().unwrap();
        let url = url::Url::parse(&link).unwrap();
        let query = url
            .query_pairs()
            .collect::<std::collections::HashMap<_, _>>();
        assert_eq!(
            query["code_challenge"],
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
        assert!(!link.contains(&auth.verifier));
        assert_eq!(query["redirect_uri"], REDIRECT_URI);
        let code = auth
            .callback_code(&format!(
                "{REDIRECT_URI}?code=code%2Bvalue&state=test-state"
            ))
            .unwrap();
        assert_eq!(code, "code+value");
        let exchange = auth.exchange_request(&code).unwrap();
        let body = url::form_urlencoded::parse(&exchange.body)
            .collect::<std::collections::HashMap<_, _>>();
        assert_eq!(body["code"], "code+value");
        assert_eq!(body["code_verifier"], auth.verifier);
        assert_eq!(body["redirect_uri"], REDIRECT_URI);
    }
    #[test]
    fn callbacks_are_bound_to_the_flow_and_expected_redirect() {
        let auth = BrowserAuthorization {
            state: "expected".into(),
            verifier: "verifier".into(),
        };
        for url in [
            "https://example.com/auth/callback?state=expected&code=secret",
            "http://localhost:1455/other?state=expected&code=secret",
            "http://localhost:1455/auth/callback?state=wrong&code=secret",
            "http://localhost:1455/auth/callback?state=expected&state=expected&code=secret",
            "http://localhost:1455/auth/callback?state=expected&code=secret&code=other",
            "http://localhost:1455/auth/callback?state=expected&error=PRIVATE_ERROR",
        ] {
            let error = auth.callback_code(url).unwrap_err();
            assert!(!error.message.contains("PRIVATE_ERROR"));
            assert!(!error.message.contains("secret"));
        }
    }
}
