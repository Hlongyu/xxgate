use bytes::Bytes;
use serde_json::Value;
use xxgate_core::{
    Error, Result,
    protocol::{EncryptedReasoningRecovery, PreparedRequest, RequestKind},
};

/// Only an explicit HTTP rejection qualifies. A message substring, SSE error,
/// failed transport, Compact or Search request must never cause another send.
pub(super) fn prepare(
    kind: RequestKind,
    request: &mut PreparedRequest,
    status: u16,
    error_body: &[u8],
) -> Result<Option<EncryptedReasoningRecovery>> {
    if kind != RequestKind::Responses || status != 400 {
        return Ok(None);
    }
    let Ok(error) = serde_json::from_slice::<Value>(error_body) else {
        return Ok(None);
    };
    if error.pointer("/error/code").and_then(Value::as_str) != Some("invalid_encrypted_content") {
        return Ok(None);
    }
    let mut body: Value = serde_json::from_slice(&request.body)
        .map_err(|_| Error::invalid("Unable to prepare encrypted reasoning recovery"))?;
    let mut changes = EncryptedReasoningRecovery::default();
    match body.get_mut("input") {
        Some(Value::Array(items)) => items.retain_mut(|item| clean_item(item, &mut changes)),
        Some(item @ Value::Object(_)) => {
            if !clean_item(item, &mut changes) {
                *item = Value::Array(vec![]);
            }
        }
        _ => {}
    }
    if changes.encrypted_fields_removed == 0 {
        return Ok(None);
    }
    request.body = Bytes::from(
        serde_json::to_vec(&body)
            .map_err(|_| Error::invalid("Unable to encode encrypted reasoning recovery"))?,
    );
    Ok(Some(changes))
}

fn clean_item(item: &mut Value, changes: &mut EncryptedReasoningRecovery) -> bool {
    if item.get("type").and_then(Value::as_str).map(str::trim) != Some("reasoning") {
        return true;
    }
    let object = item.as_object_mut().expect("reasoning item is an object");
    if object.remove("encrypted_content").is_none() {
        return true;
    }
    changes.encrypted_fields_removed += 1;
    if object.get("content") == Some(&Value::Null) {
        object.remove("content");
        changes.null_content_fields_removed += 1;
    }
    // Preserve IDs, useful summaries, non-null content and unknown metadata.
    if object.len() == 1 {
        changes.empty_reasoning_items_removed += 1;
        false
    } else {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::{HeaderMap, Method};
    use serde_json::json;

    const ERROR: &[u8] =
        br#"{"error":{"code":"invalid_encrypted_content","message":"PRIVATE_ERROR"}}"#;
    fn request(input: Value) -> PreparedRequest {
        PreparedRequest {
            method: Method::POST,
            url: "https://example.invalid/responses".into(),
            headers: HeaderMap::new(),
            body: Bytes::from(json!({"model":"test","input":input,"tools":[{"type":"function","name":"test"}],"client_metadata":{"session_id":"s"}}).to_string()),
            account_id: None,
            profile_version: 1,
            tls_backend: "rustls".into(),
        }
    }

    #[test]
    fn removes_only_encrypted_reasoning_and_retains_other_history() {
        let input = json!([
            {"type":"reasoning","id":"rs_1","encrypted_content":"PRIVATE_CIPHER","content":null,"summary":[{"type":"summary_text","text":"PRIVATE_SUMMARY"}],"extension":{"keep":true}},
            {"type":"reasoning","encrypted_content":"PRIVATE_EMPTY","content":null},
            {"type":"compaction","encrypted_content":"PRIVATE_COMPACTION"},
            {"type":"function_call","encrypted_function_args":"PRIVATE_ARGS","call_id":"call_a"},
            {"type":"function_call_output","call_id":"call_a","output":"PRIVATE_RESULT"},
            {"type":"reasoning","summary":[],"content":null},
            {"role":"user","content":"PRIVATE_PROMPT"}
        ]);
        let mut req = request(input.clone());
        let original: Value = serde_json::from_slice(&req.body).unwrap();
        let changes = prepare(RequestKind::Responses, &mut req, 400, ERROR)
            .unwrap()
            .unwrap();
        assert_eq!(changes.encrypted_fields_removed, 2);
        assert_eq!(changes.null_content_fields_removed, 2);
        assert_eq!(changes.empty_reasoning_items_removed, 1);
        assert!(
            !serde_json::to_string(&changes)
                .unwrap()
                .contains("PRIVATE_")
        );
        let result: Value = serde_json::from_slice(&req.body).unwrap();
        let mut expected = original;
        expected["input"].as_array_mut().unwrap().remove(1);
        expected["input"][0]
            .as_object_mut()
            .unwrap()
            .remove("encrypted_content");
        expected["input"][0]
            .as_object_mut()
            .unwrap()
            .remove("content");
        assert_eq!(result, expected);
        assert!(
            prepare(RequestKind::Responses, &mut req, 400, ERROR)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn strict_error_gate_never_changes_unrelated_requests() {
        for (kind, status, error) in [
            (RequestKind::Responses, 200, ERROR),
            (RequestKind::Responses, 429, ERROR),
            (RequestKind::Responses, 500, ERROR),
            (RequestKind::Compact, 400, ERROR),
            (RequestKind::Search, 400, ERROR),
            (
                RequestKind::Responses,
                400,
                br#"{"error":{"message":"invalid_encrypted_content"}}"#.as_slice(),
            ),
            (
                RequestKind::Responses,
                400,
                br#"{"error":{"code":"invalid_request_error"}}"#.as_slice(),
            ),
            (RequestKind::Responses, 400, b"invalid JSON".as_slice()),
        ] {
            let mut req = request(json!([{"type":"reasoning","encrypted_content":"PRIVATE"}]));
            let before = req.body.clone();
            assert!(prepare(kind, &mut req, status, error).unwrap().is_none());
            assert_eq!(req.body, before);
        }
        let mut req = request(
            json!([{"type":"compaction","encrypted_content":"PRIVATE"},{"type":"reasoning","content":null}]),
        );
        let before = req.body.clone();
        assert!(
            prepare(RequestKind::Responses, &mut req, 400, ERROR)
                .unwrap()
                .is_none()
        );
        assert_eq!(req.body, before);
    }

    #[test]
    fn single_reasoning_object_keeps_summary_and_nonnull_content() {
        let mut req = request(
            json!({"type":"reasoning","encrypted_content":"PRIVATE","summary":[],"content":[{"text":"retained"}]}),
        );
        prepare(RequestKind::Responses, &mut req, 400, ERROR)
            .unwrap()
            .unwrap();
        let value: Value = serde_json::from_slice(&req.body).unwrap();
        assert_eq!(
            value["input"],
            json!({"type":"reasoning","summary":[],"content":[{"text":"retained"}]})
        );
    }
}
