//! Recognize the inbound caller before any outbound Codex profile is applied.
//! Add client-specific recognizers to this registry as verified signatures become
//! available (for example OpenCode or pi); unknown callers remain valid.
use http::HeaderMap;
use serde_json::Value;
use xxgate_core::clients::{ClientOrigin, ClientSource};

type Recognizer = fn(&HeaderMap, &Value) -> Option<ClientOrigin>;
const RECOGNIZERS: &[Recognizer] = &[codex];

pub fn detect(headers: &HeaderMap, body: &Value) -> ClientOrigin {
    RECOGNIZERS
        .iter()
        .find_map(|recognizer| recognizer(headers, body))
        .unwrap_or_default()
}

fn known(rule: &str, evidence: Vec<String>) -> ClientOrigin {
    ClientOrigin {
        source: ClientSource::Codex,
        evidence,
        rule: rule.into(),
        version: 1,
    }
}

fn identity(value: Option<&Value>) -> bool {
    value
        .and_then(Value::as_str)
        .is_some_and(|s| !s.trim().is_empty() && s.len() <= 512 && !s.chars().any(char::is_control))
}

fn turn_signature(meta: &Value) -> bool {
    ["session_id", "thread_id", "turn_id"]
        .iter()
        .all(|k| identity(meta.get(k)))
        && [
            "installation_id",
            "x-codex-installation-id",
            "window_id",
            "x-codex-window-id",
        ]
        .iter()
        .any(|k| identity(meta.get(k)))
}

fn codex(headers: &HeaderMap, body: &Value) -> Option<ClientOrigin> {
    if let Some(ua) = headers
        .get("user-agent")
        .and_then(|h| h.to_str().ok())
        .filter(|s| s.len() <= 512)
        && ua
            .split(" (")
            .next()
            .and_then(|prefix| prefix.rsplit_once('/'))
            .is_some_and(|(product, version)| {
                let known = matches!(product, "codex_cli_rs" | "codex-tui" | "codex_vscode")
                    || product
                        .strip_prefix("Codex ")
                        .is_some_and(|name| !name.is_empty() && name.len() <= 80);
                known
                    && !version.is_empty()
                    && version.len() <= 64
                    && version
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b".-_+".contains(&b))
            })
    {
        return Some(known("codex_user_agent", vec!["headers.user-agent".into()]));
    }
    for (path, text) in [
        (
            "headers.x-codex-turn-metadata",
            headers
                .get("x-codex-turn-metadata")
                .and_then(|h| h.to_str().ok()),
        ),
        (
            "body.client_metadata.x-codex-turn-metadata",
            body.pointer("/client_metadata/x-codex-turn-metadata")
                .and_then(Value::as_str),
        ),
    ] {
        if let Some(text) = text.filter(|s| s.len() <= 128 * 1024)
            && let Ok(meta) = serde_json::from_str::<Value>(text)
            && turn_signature(&meta)
        {
            return Some(known("codex_turn_metadata", vec![path.into()]));
        }
    }
    if let Some(meta) = body.get("client_metadata")
        && turn_signature(meta)
        && identity(meta.get("x-codex-installation-id"))
    {
        return Some(known(
            "codex_client_metadata",
            vec!["body.client_metadata".into()],
        ));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn only_specific_inbound_signatures_identify_codex() {
        let mut headers = HeaderMap::new();
        for ua in [
            "Go-http-client/1.1",
            "opencode/1.0",
            "pi/0.1",
            "not-codex_cli_rs/1",
            "codex_cli_rs/",
        ] {
            headers.insert("user-agent", ua.parse().unwrap());
            assert_eq!(detect(&headers,&json!({"model":"codex","prompt_cache_key":"s","client_metadata":{"session_id":"s","thread_id":"t"}})).source,ClientSource::Unknown);
        }
        for ua in [
            "codex_cli_rs/0.153.4 (Mac OS)",
            "codex_vscode/0.153.4",
            "codex-tui/0.153.4",
            "Codex Desktop/26.1 (Mac OS)",
        ] {
            headers.insert("user-agent", ua.parse().unwrap());
            assert_eq!(detect(&headers, &json!({})).source, ClientSource::Codex);
        }
        headers.insert("user-agent", "Go-http-client/1.1".parse().unwrap());
        let meta = json!({"session_id":"s","thread_id":"t","turn_id":"turn","installation_id":"installation"});
        assert_eq!(
            detect(
                &headers,
                &json!({"client_metadata":{"x-codex-turn-metadata":meta.to_string()}})
            )
            .source,
            ClientSource::Codex
        );
        headers.insert("x-codex-turn-metadata", meta.to_string().parse().unwrap());
        assert_eq!(detect(&headers, &json!({})).source, ClientSource::Codex);
    }

    #[test]
    fn recognition_is_bounded_and_records_no_content() {
        let body = json!({"input":"PRIVATE_PROMPT","client_metadata":{"x-codex-turn-metadata":"PRIVATE_BAD_JSON"}});
        let detected = detect(&HeaderMap::new(), &body);
        assert_eq!(detected.source, ClientSource::Unknown);
        assert!(
            !serde_json::to_string(&detected)
                .unwrap()
                .contains("PRIVATE_")
        );
    }
}
