use serde_json::{Map, Value};
use xxgate_core::types::OriginalUpstreamError;

/// Save diagnostic fields before normalizing the error for the public protocol.
/// Never copy a response's input/output, headers, credentials or whole JSON body.
pub(super) fn capture(value: Option<&Value>, body: &[u8]) -> OriginalUpstreamError {
    let mut fields = Map::new();
    let mut truncated = false;
    if let Some(value) = value {
        for name in ["code", "type", "message", "param", "detail"] {
            let nested = value.get("error").and_then(|e| e.get(name));
            let field = nested.filter(|v| !v.is_null()).or_else(|| value.get(name));
            if let Some(field) = field {
                insert(&mut fields, name, field, &mut truncated);
            }
        }
        if let Some(reason) = value.pointer("/incomplete_details/reason") {
            insert(&mut fields, "incomplete_reason", reason, &mut truncated);
        }
        if let Some(message) = value.get("error").filter(|v| v.is_string()) {
            insert(&mut fields, "message", message, &mut truncated);
        }
    } else {
        let text = String::from_utf8_lossy(&body[..body.len().min(16_384)]);
        truncated = body.len() > 16_384;
        insert(
            &mut fields,
            "body",
            &Value::String(text.into_owned()),
            &mut truncated,
        );
    }
    OriginalUpstreamError {
        source: "http".into(),
        fields: Value::Object(fields),
        truncated,
    }
}

fn insert(fields: &mut Map<String, Value>, name: &str, value: &Value, truncated: &mut bool) {
    // Preserve scalar diagnostic values, including unknown codes and numeric codes.
    if let Some(text) = value.as_str() {
        let limit = if matches!(name, "message" | "detail" | "body") {
            4096
        } else {
            256
        };
        let saved: String = text.chars().take(limit).collect();
        *truncated |= saved.len() < text.len();
        fields.insert(name.into(), saved.into());
    } else if value.is_number() || value.is_boolean() || value.is_null() {
        fields.insert(name.into(), value.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn unknown_codes_null_fallback_and_utf8_limits_preserve_diagnostics_only() {
        let value = json!({"code":"fallback", "error":{"code":null,"type":"novel_error","message":"错".repeat(5000),"param":"tools[0].name"},"input":"PRIVATE_INPUT","output":"PRIVATE_OUTPUT"});
        let original = capture(Some(&value), &[]);
        assert_eq!(original.fields["code"], "fallback");
        assert_eq!(original.fields["type"], "novel_error");
        assert_eq!(original.fields["param"], "tools[0].name");
        assert_eq!(
            original.fields["message"].as_str().unwrap().chars().count(),
            4096
        );
        assert!(original.truncated);
        assert!(
            !serde_json::to_string(&original)
                .unwrap()
                .contains("PRIVATE_")
        );
        assert!(!format!("{original:?}").contains("novel_error"));
        let old: xxgate_core::types::UpstreamError =
            serde_json::from_value(json!({"code":"server_error","reason":null,"param":null}))
                .unwrap();
        assert!(old.original.is_none());
    }
}
