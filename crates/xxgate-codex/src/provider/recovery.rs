use bytes::Bytes;
use serde_json::{Value, json};
use xxgate_core::{
    Error, Result,
    protocol::{EncryptedContentError, EncryptedContentRecovery, PreparedRequest, RequestKind},
};

/// HTTP recovery requires an explicit 400. SSE recovery is separately gated by
/// the decoder and by the gateway's undispatched preamble buffer.
pub(super) fn prepare(
    kind: RequestKind,
    request: &mut PreparedRequest,
    status: u16,
    error_body: &[u8],
) -> Result<Option<EncryptedContentRecovery>> {
    if kind != RequestKind::Responses || status != 400 {
        return Ok(None);
    }
    let Ok(error) = serde_json::from_slice::<Value>(error_body) else {
        return Ok(None);
    };
    let Some(error) = rejection(&error) else {
        return Ok(None);
    };
    clean(request, error)
}

pub(super) fn rejection(value: &Value) -> Option<EncryptedContentError> {
    if value
        .pointer("/error/code")
        .or_else(|| value.get("code"))
        .and_then(Value::as_str)
        != Some("invalid_encrypted_content")
    {
        return None;
    }
    let message = value
        .pointer("/error/message")
        .or_else(|| value.get("message"))
        .and_then(Value::as_str)
        .unwrap_or("");
    Some(
        if message == "Encrypted function output content could not be decrypted or decoded." {
            EncryptedContentError::ToolOutput
        } else {
            EncryptedContentError::Reasoning
        },
    )
}

fn tool_output(item: &Value) -> bool {
    matches!(
        item.get("type").and_then(Value::as_str),
        Some("function_call_output" | "custom_tool_call_output")
    )
}

pub(super) fn has_recoverable_input(request: &PreparedRequest) -> bool {
    let Ok(body) = serde_json::from_slice::<Value>(&request.body) else {
        return false;
    };
    let has = |item: &Value| {
        (item["type"] == "reasoning" && item.get("encrypted_content").is_some())
            || (tool_output(item)
                && item["output"]
                    .as_array()
                    .is_some_and(|parts| parts.iter().any(|p| p["type"] == "encrypted_content")))
    };
    match body.get("input") {
        Some(Value::Array(items)) => items.iter().any(has),
        Some(item @ Value::Object(_)) => has(item),
        _ => false,
    }
}

pub(super) fn clean(
    request: &mut PreparedRequest,
    error: EncryptedContentError,
) -> Result<Option<EncryptedContentRecovery>> {
    let mut body: Value = serde_json::from_slice(&request.body)
        .map_err(|_| Error::invalid("Unable to prepare encrypted reasoning recovery"))?;
    let mut changes = EncryptedContentRecovery {
        error_kind: error,
        ..Default::default()
    };
    match body.get_mut("input") {
        Some(Value::Array(items)) => items.retain_mut(|item| clean_item(item, &mut changes)),
        Some(item @ Value::Object(_)) => {
            if !clean_item(item, &mut changes) {
                *item = Value::Array(vec![]);
            }
        }
        _ => {}
    }
    if changes.encrypted_fields_removed == 0 && changes.encrypted_tool_parts_replaced == 0 {
        return Ok(None);
    }
    request.body = Bytes::from(
        serde_json::to_vec(&body)
            .map_err(|_| Error::invalid("Unable to encode encrypted reasoning recovery"))?,
    );
    Ok(Some(changes))
}

fn clean_item(item: &mut Value, changes: &mut EncryptedContentRecovery) -> bool {
    if matches!(changes.error_kind, EncryptedContentError::ToolOutput) {
        if tool_output(item)
            && let Some(parts) = item.get_mut("output").and_then(Value::as_array_mut)
        {
            let before = changes.encrypted_tool_parts_replaced;
            for part in parts {
                if part["type"] == "encrypted_content" {
                    // Preserve call identity and every usable text/image part.
                    // Never fabricate a successful result or silently lose the
                    // fact that an already executed tool's result is unavailable.
                    *part = json!({"type":"input_text","text":"[XXGate: An encrypted part of this historical tool result was rejected by the upstream and is unavailable. The tool may already have executed. Do not infer success or failure, or repeat side-effecting actions solely because this result is unavailable.]"});
                    changes.encrypted_tool_parts_replaced += 1;
                }
            }
            if changes.encrypted_tool_parts_replaced > before {
                changes.tool_outputs_changed += 1;
            }
        }
        return true;
    }
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

    #[test]
    fn tool_recovery_marks_unavailable_parts_and_preserves_call_identity_and_plaintext() {
        let original = json!([
            {"type":"function_call","id":"fc_a","call_id":"a","encrypted_function_args":["PRIVATE_ARGS"],"arguments":"{}"},
            {"type":"function_call_output","call_id":"a","id":"fco_a","output":[{"type":"input_text","text":"PRIVATE_TEXT"},{"type":"encrypted_content","encrypted_content":"PRIVATE_CIPHER"},{"type":"input_image","image_url":"PRIVATE_IMAGE"}]},
            {"type":"custom_tool_call_output","call_id":"b","output":[{"type":"encrypted_content","encrypted_content":"PRIVATE_ONLY_CIPHER"}],"extension":true},
            {"type":"reasoning","encrypted_content":"PRIVATE_REASONING","summary":[]},
            {"type":"compaction","encrypted_content":"PRIVATE_COMPACTION"},
            {"type":"function_call_output","call_id":"c","output":"PRIVATE_PLAIN_STRING"}
        ]);
        let mut req = request(original.clone());
        assert!(has_recoverable_input(&req));
        let changes = prepare(RequestKind::Responses, &mut req, 400, br#"{"error":{"code":"invalid_encrypted_content","message":"Encrypted function output content could not be decrypted or decoded."}}"#).unwrap().unwrap();
        assert_eq!(changes.tool_outputs_changed, 2);
        assert_eq!(changes.encrypted_tool_parts_replaced, 2);
        assert_eq!(changes.encrypted_fields_removed, 0);
        let after: Value = serde_json::from_slice(&req.body).unwrap();
        let mut expected = original;
        let marker = after["input"][1]["output"][1].clone();
        assert_eq!(marker["type"], "input_text");
        assert!(
            marker["text"]
                .as_str()
                .unwrap()
                .contains("may already have executed")
        );
        expected[1]["output"][1] = marker.clone();
        expected[2]["output"][0] = marker;
        assert_eq!(after["input"], expected);
        assert!(
            !serde_json::to_string(&changes)
                .unwrap()
                .contains("PRIVATE_")
        );
        assert!(
            clean(&mut req, EncryptedContentError::ToolOutput)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn exact_error_code_and_specific_tool_error_select_cleanup_without_guessing() {
        assert!(rejection(&json!({"error":{"message":"invalid_encrypted_content"}})).is_none());
        assert!(matches!(
            rejection(
                &json!({"code":"invalid_encrypted_content","message":"Encrypted function output content could not be decrypted or decoded."})
            ),
            Some(EncryptedContentError::ToolOutput)
        ));
        let mut req = request(
            json!([{"type":"function_call_output","output":"ordinary"},{"type":"message","content":[{"type":"encrypted_content","encrypted_content":"KEEP"}]}]),
        );
        let before = req.body.clone();
        assert!(!has_recoverable_input(&req));
        assert!(
            clean(&mut req, EncryptedContentError::ToolOutput)
                .unwrap()
                .is_none()
        );
        assert_eq!(req.body, before);
    }
}
