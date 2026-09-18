use http::HeaderMap;
use serde_json::{Map, Value};
use xxgate_core::{
    Error, Result,
    protocol::{Compaction, CompactionMethod, GatewayRequest, RequestKind},
};

pub fn parse(headers: &HeaderMap, body: Value) -> Result<GatewayRequest> {
    crate::ingress::parse(headers, body, RequestKind::Compact)
}

pub fn inspect(headers: &HeaderMap, body: &Value) -> Result<Option<Compaction>> {
    operation(
        body,
        &crate::ingress::metadata(body, headers)?,
        RequestKind::Responses,
    )
}

pub(crate) fn operation(
    body: &Value,
    sources: &[Value],
    kind: RequestKind,
) -> Result<Option<Compaction>> {
    let metadata = turn_metadata(sources)?;
    let method = if kind == RequestKind::Compact {
        CompactionMethod::Compact
    } else if kind == RequestKind::Responses
        && (body
            .get("input")
            .and_then(Value::as_array)
            .is_some_and(|items| {
                items
                    .iter()
                    .any(|item| item["type"] == "compaction_trigger")
            })
            || (metadata.get("request_kind").and_then(Value::as_str) == Some("compaction")
                && metadata
                    .get("compaction")
                    .and_then(|v| v.get("implementation"))
                    .and_then(Value::as_str)
                    == Some("responses_compaction_v2")))
    {
        CompactionMethod::RemoteV2
    } else {
        // A historical compaction item and Local summary metadata are not V2 triggers.
        return Ok(None);
    };
    Ok(Some(Compaction {
        method,
        output_observed: None,
    }))
}

/// Project known operation metadata when sub2api carries it only in the body.
/// Validate conflicts just like identity fields, and never copy arbitrary text to headers.
pub(crate) fn turn_metadata(sources: &[Value]) -> Result<Map<String, Value>> {
    let mut result = Map::new();
    if let Some(kind) = crate::ingress::coherent(&HeaderMap::new(), None, sources, "request_kind")?
    {
        result.insert("request_kind".into(), Value::String(kind));
    }
    let mut compaction = Map::new();
    for source in sources {
        let Some(value) = source.get("compaction").filter(|v| !v.is_null()) else {
            continue;
        };
        // Older clients may carry an unrelated flat string with this name.
        let Some(object) = value.as_object() else {
            continue;
        };
        for field in ["trigger", "reason", "implementation", "phase", "strategy"] {
            let Some(value) = object.get(field) else {
                continue;
            };
            let text = value
                .as_str()
                .filter(|s| {
                    !s.is_empty()
                        && s.len() <= 128
                        && s.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_')
                })
                .ok_or_else(|| Error::invalid("Invalid compaction metadata"))?;
            if compaction
                .get(field)
                .is_some_and(|previous| previous != text)
            {
                return Err(Error::new(
                    400,
                    "compaction_metadata_conflict",
                    "Compaction metadata fields disagree",
                ));
            }
            compaction.insert(field.into(), Value::String(text.into()));
        }
    }
    if !compaction.is_empty() {
        result.insert("compaction".into(), Value::Object(compaction));
    }
    Ok(result)
}

pub(crate) fn has_output(items: &[Value]) -> bool {
    items.iter().any(|item| item["type"] == "compaction")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use xxgate_core::protocol::IngressAdapter;

    #[test]
    fn identifies_current_v2_operations_without_misclassifying_history_or_local_summary() {
        for (input, meta, expected) in [
            (json!([{"type":"compaction_trigger"}]), json!({}), true),
            (
                json!([]),
                json!({"request_kind":"compaction","compaction":{"implementation":"responses_compaction_v2"}}),
                true,
            ),
            (
                json!([{"type":"compaction","encrypted_content":"PRIVATE_HISTORY"}]),
                json!({}),
                false,
            ),
            (
                json!([{"type":"message","content":[{"type":"compaction_trigger"}]}]),
                json!({}),
                false,
            ),
            (
                json!([]),
                json!({"request_kind":"compaction","compaction":{"implementation":"responses"}}),
                false,
            ),
            (json!([]), json!({"request_kind":"compaction"}), false),
        ] {
            for header_only in [false, true] {
                let mut headers = HeaderMap::new();
                let mut body = json!({"model":"m","input":input});
                if header_only {
                    headers.insert("x-codex-turn-metadata", meta.to_string().parse().unwrap());
                } else {
                    body["client_metadata"] = json!({"x-codex-turn-metadata":meta.to_string()});
                }
                let parsed = crate::ingress::ResponsesIngress
                    .parse(&headers, body.clone())
                    .unwrap();
                assert_eq!(parsed.compaction.is_some(), expected);
                assert_eq!(parsed.document.value, body);
                assert_eq!(parsed.kind, RequestKind::Responses);
            }
        }
    }

    #[test]
    fn compact_rejects_streaming_and_response_only_options() {
        for extra in [
            json!({"stream":true}),
            json!({"previous_response_id":"resp_old"}),
            json!({"background":true}),
            json!({"context_management":[]}),
        ] {
            let mut body = json!({"model":"m","input":[]});
            body.as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            assert!(parse(&HeaderMap::new(), body).is_err());
        }
        let parsed = parse(
            &HeaderMap::new(),
            json!({"model":"m","input":[],"stream":false,"access_programs":[]}),
        )
        .unwrap();
        assert_eq!(parsed.kind, RequestKind::Compact);
        assert!(!parsed.stream);
        assert!(parsed.stateless);
    }

    #[test]
    fn conflicting_operation_metadata_is_rejected() {
        assert_eq!(turn_metadata(&[
            json!({"request_kind":"compaction","compaction":{"implementation":"responses_compaction_v2"}}),
            json!({"request_kind":"compaction","compaction":{"implementation":"responses"}}),
        ]).unwrap_err().code, "compaction_metadata_conflict");
    }
}
