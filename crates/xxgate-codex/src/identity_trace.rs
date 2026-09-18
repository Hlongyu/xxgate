use http::HeaderMap;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use xxgate_core::identity::{IdentifierRewrite, RequestRewrite};

const IDENTIFIERS: &[&str] = &[
    "session_id",
    "session-id",
    "conversation_id",
    "conversation-id",
    "thread_id",
    "thread-id",
    "turn_id",
    "window_id",
    "context_window_id",
    "installation_id",
    "parent_thread_id",
    "forked_from_thread_id",
    "parent_turn_id",
    "root_turn_id",
    "x-codex-installation-id",
    "x-codex-window-id",
    "x-codex-parent-thread-id",
];
const HEADERS: &[&str] = &[
    "session-id",
    "session_id",
    "conversation-id",
    "conversation_id",
    "thread-id",
    "thread_id",
    "x-client-request-id",
    "x-codex-installation-id",
    "x-codex-window-id",
    "x-codex-parent-thread-id",
];
const MAX_ENTRIES: usize = 2000;

fn identifier(value: &str) -> Option<&str> {
    (value.len() <= 512 && !value.chars().any(char::is_control)).then_some(value)
}

fn metadata_ids(out: &mut BTreeMap<String, String>, prefix: &str, metadata: &Value) {
    for name in IDENTIFIERS {
        if let Some(value) = metadata
            .get(name)
            .and_then(Value::as_str)
            .and_then(identifier)
        {
            out.insert(format!("{prefix}.{name}"), value.into());
        }
    }
}

// Record only explicitly enumerated identifier fields. Never collect arbitrary
// headers, metadata, tool arguments, encrypted contents or request text.
pub(crate) fn client_identifiers(headers: &HeaderMap, body: &Value) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for name in HEADERS {
        if let Some(value) = headers
            .get(*name)
            .and_then(|v| v.to_str().ok())
            .and_then(identifier)
        {
            out.insert(format!("headers.{name}"), value.into());
        }
    }
    if let Some(value) = headers
        .get("x-codex-turn-metadata")
        .and_then(|v| serde_json::from_slice::<Value>(v.as_bytes()).ok())
    {
        metadata_ids(&mut out, "headers.x-codex-turn-metadata", &value);
    }
    if let Some(meta) = body.get("client_metadata") {
        metadata_ids(&mut out, "body.client_metadata", meta);
        if let Some(value) = meta
            .get("x-codex-turn-metadata")
            .and_then(Value::as_str)
            .and_then(|v| serde_json::from_str::<Value>(v).ok())
        {
            metadata_ids(
                &mut out,
                "body.client_metadata.x-codex-turn-metadata",
                &value,
            );
        }
    }
    if let Some(value) = body
        .get("prompt_cache_key")
        .and_then(Value::as_str)
        .and_then(identifier)
    {
        out.insert("body.prompt_cache_key".into(), value.into());
    }
    out
}

pub(crate) fn identity_headers(headers: &HeaderMap) -> HeaderMap {
    let mut selected = HeaderMap::new();
    for name in HEADERS
        .iter()
        .copied()
        .chain(std::iter::once("x-codex-turn-metadata"))
    {
        for value in headers.get_all(name) {
            selected.append(name, value.clone());
        }
    }
    selected
}

fn push(
    trace: &mut RequestRewrite,
    field: String,
    before: Option<&str>,
    after: Option<&str>,
    body_id: bool,
) {
    if trace.entries.len() >= MAX_ENTRIES
        || before.is_some_and(|v| identifier(v).is_none())
        || after.is_some_and(|v| identifier(v).is_none())
    {
        trace.omitted += 1;
        return;
    }
    let action = if before == after {
        "unchanged"
    } else if before.is_none() {
        "added"
    } else if after.is_none() {
        "removed"
    } else if body_id {
        "alias_restored"
    } else {
        "rewritten"
    };
    trace.entries.push(IdentifierRewrite {
        field,
        before: before.map(str::to_owned),
        after: after.map(str::to_owned),
        action: action.into(),
    });
}

pub(crate) fn compare(
    incoming: &BTreeMap<String, String>,
    source: &Value,
    headers: &HeaderMap,
    body: &Value,
) -> RequestRewrite {
    let outgoing = client_identifiers(headers, body);
    let mut trace = RequestRewrite::default();
    let fields: BTreeSet<_> = incoming.keys().chain(outgoing.keys()).collect();
    for field in fields {
        push(
            &mut trace,
            field.clone(),
            incoming.get(field).map(String::as_str),
            outgoing.get(field).map(String::as_str),
            false,
        );
    }
    if let Some(items) = source.get("input").and_then(Value::as_array) {
        for (index, item) in items.iter().enumerate() {
            for field in ["id", "call_id"] {
                let before = item.get(field).and_then(Value::as_str);
                let after = body
                    .get("input")
                    .and_then(|v| v.get(index))
                    .and_then(|v| v.get(field))
                    .and_then(Value::as_str);
                if before.is_some() || after.is_some() {
                    push(
                        &mut trace,
                        format!("body.input[{index}].{field}"),
                        before,
                        after,
                        true,
                    );
                }
            }
        }
    }
    trace
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn compares_exact_identifier_locations_without_collecting_user_content() {
        let incoming = HeaderMap::from_iter([
            (
                "session-id".parse().unwrap(),
                "client-session".parse().unwrap(),
            ),
            (
                "authorization".parse().unwrap(),
                "Bearer PRIVATE_TOKEN".parse().unwrap(),
            ),
            (
                "x-codex-turn-metadata".parse().unwrap(),
                r#"{"thread_id":"client-thread","notes":"PRIVATE_METADATA"}"#
                    .parse()
                    .unwrap(),
            ),
        ]);
        let source = json!({"input":[{"id":"rs_alias","encrypted_content":"PRIVATE_CIPHERTEXT","content":"PRIVATE_TEXT","arguments":{"id":"PRIVATE_TOOL_ID"}}],"prompt_cache_key":"old-cache","client_metadata":{"thread_id":"client-thread","notes":"PRIVATE_METADATA","x-codex-turn-metadata":r#"{"thread_id":"client-thread","notes":"PRIVATE_METADATA"}"#}});
        let mut body = source.clone();
        body["input"][0]["id"] = json!("rs_original");
        body["prompt_cache_key"] = json!("upstream-session");
        body["client_metadata"] =
            json!({"thread_id":"upstream-thread","installation_id":"account-installation"});
        let outgoing = HeaderMap::from_iter([(
            "session-id".parse().unwrap(),
            "upstream-session".parse().unwrap(),
        )]);
        let trace = compare(
            &client_identifiers(&incoming, &source),
            &source,
            &outgoing,
            &body,
        );
        let rows = &trace.entries;
        let session = rows
            .iter()
            .find(|r| r.field == "headers.session-id")
            .unwrap();
        assert_eq!(session.before.as_deref(), Some("client-session"));
        assert_eq!(session.after.as_deref(), Some("upstream-session"));
        assert_eq!(session.action, "rewritten");
        let alias = rows.iter().find(|r| r.field == "body.input[0].id").unwrap();
        assert_eq!(alias.before.as_deref(), Some("rs_alias"));
        assert_eq!(alias.after.as_deref(), Some("rs_original"));
        assert_eq!(alias.action, "alias_restored");
        assert!(
            rows.iter()
                .any(|r| r.field == "body.client_metadata.installation_id" && r.action == "added")
        );
        assert!(
            rows.iter()
                .any(|r| r.field == "headers.x-codex-turn-metadata.thread_id"
                    && r.action == "removed")
        );
        assert_eq!(trace.omitted, 0);
        let saved = serde_json::to_string(&trace).unwrap();
        for forbidden in [
            "PRIVATE_",
            "authorization",
            "encrypted_content",
            "arguments",
            "notes",
        ] {
            assert!(!saved.contains(forbidden));
        }
    }
    #[test]
    fn oversized_and_excess_identifiers_are_counted_without_altering_forwarded_data() {
        let input: Vec<_> = (0..MAX_ENTRIES + 3)
            .map(|i| json!({"id":format!("msg_{i}")}))
            .collect();
        let body = json!({"input":input});
        let trace = compare(&BTreeMap::new(), &body, &HeaderMap::new(), &body);
        assert_eq!(trace.entries.len(), MAX_ENTRIES);
        assert_eq!(trace.omitted, 3);
        assert!(trace.entries.iter().all(|r| r.action == "unchanged"));
        let body = json!({"input":[{"id":"x".repeat(513)}]});
        let trace = compare(&BTreeMap::new(), &body, &HeaderMap::new(), &body);
        assert!(trace.entries.is_empty());
        assert_eq!(trace.omitted, 1);
        assert_eq!(body["input"][0]["id"].as_str().unwrap().len(), 513);
    }
}
