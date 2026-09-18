use crate::wire::{self, FORMAT};
use bytes::Bytes;
use http::HeaderMap;
use serde_json::{Value, json};
use uuid::Uuid;
use xxgate_core::{
    Error, Result,
    identity::ClientIdentity,
    protocol::{GatewayRequest, IngressAdapter, ProtocolDocument, RequestKind},
};

pub struct ResponsesIngress;

pub(crate) fn metadata(body: &Value, headers: &HeaderMap) -> Result<Vec<Value>> {
    let mut result = vec![];
    if let Some(meta) = body.get("client_metadata") {
        if !meta.is_object() {
            return Err(Error::invalid("client_metadata must be an object"));
        }
        result.push(meta.clone());
        if let Some(nested) = meta.get("x-codex-turn-metadata") {
            let text = nested
                .as_str()
                .ok_or_else(|| Error::invalid("Codex turn metadata must be a JSON string"))?;
            if text.len() > 128 * 1024 {
                return Err(Error::invalid("Codex turn metadata is too large"));
            }
            result.push(
                serde_json::from_str(text)
                    .map_err(|_| Error::invalid("Codex turn metadata is invalid JSON"))?,
            );
        }
    }
    if let Some(value) = headers.get("x-codex-turn-metadata") {
        if headers.get_all("x-codex-turn-metadata").iter().count() != 1 {
            return Err(Error::invalid("Duplicate Codex metadata headers"));
        }
        result.push(
            serde_json::from_slice(value.as_bytes())
                .map_err(|_| Error::invalid("Codex metadata header is invalid JSON"))?,
        );
    }
    if result.iter().any(|v| !v.is_object()) {
        return Err(Error::invalid("Codex metadata must be a JSON object"));
    }
    // Flat compatibility aliases and canonical nested fields denote the same identity.
    let aliases = result
        .iter()
        .flat_map(|source| {
            [
                ("x-codex-window-id", "window_id"),
                ("x-codex-parent-thread-id", "parent_thread_id"),
                ("x-codex-installation-id", "installation_id"),
            ]
            .into_iter()
            .filter_map(move |(alias, canonical)| {
                source.get(alias).map(|value| json!({canonical:value}))
            })
        })
        .collect::<Vec<_>>();
    result.extend(aliases);
    Ok(result)
}

pub(crate) fn coherent(
    headers: &HeaderMap,
    header: Option<&str>,
    sources: &[Value],
    field: &str,
) -> Result<Option<String>> {
    let mut values = vec![];
    if let Some(header) = header {
        for v in headers.get_all(header) {
            values.push(
                v.to_str()
                    .map_err(|_| Error::invalid("Invalid identity header"))?
                    .to_owned(),
            );
        }
    }
    for source in sources {
        if let Some(v) = source.get(field)
            && !v.is_null()
        {
            values.push(
                v.as_str()
                    .ok_or_else(|| Error::invalid("Identity fields must be strings"))?
                    .to_owned(),
            );
        }
    }
    if values
        .iter()
        .any(|v| v.is_empty() || v.len() > 512 || v.chars().any(char::is_control))
    {
        return Err(Error::invalid(
            "Invalid identity field length or characters",
        ));
    }
    if let Some(first) = values.first()
        && values.iter().any(|v| v != first)
    {
        return Err(Error::new(
            400,
            "identity_conflict",
            "Identity fields disagree between headers and request metadata",
        ));
    }
    Ok(values.into_iter().next())
}

pub(crate) fn parse(headers: &HeaderMap, body: Value, kind: RequestKind) -> Result<GatewayRequest> {
    let client_origin = crate::client_source::detect(headers, &body);
    let client_metadata_present = body.get("client_metadata").is_some();
    if !body.is_object() {
        return Err(Error::invalid("Request body must be an object"));
    }
    let allowed: &[&str] = if kind == RequestKind::Compact {
        &[
            "model",
            "input",
            "instructions",
            "tools",
            "parallel_tool_calls",
            "reasoning",
            "service_tier",
            "prompt_cache_key",
            "text",
            "client_metadata",
            "access_programs",
            "stream",
        ]
    } else {
        &[
            "model",
            "input",
            "instructions",
            "tools",
            "tool_choice",
            "parallel_tool_calls",
            "reasoning",
            "store",
            "stream",
            "stream_options",
            "include",
            "service_tier",
            "prompt_cache_key",
            "text",
            "client_metadata",
            "max_output_tokens",
            "temperature",
            "top_p",
            "truncation",
            "previous_response_id",
            "background",
            "metadata",
        ]
    };
    if body
        .as_object()
        .is_some_and(|o| o.keys().any(|k| !allowed.contains(&k.as_str())))
    {
        return Err(Error::new(
            400,
            "unsupported_parameter",
            "The request contains a parameter this gateway does not support",
        ));
    }
    if body.get("background").and_then(Value::as_bool) == Some(true) {
        return Err(Error::new(
            400,
            "unsupported_background",
            "Background responses are not supported",
        ));
    }
    if body
        .get("previous_response_id")
        .is_some_and(|v| !v.is_null())
    {
        return Err(Error::new(
            400,
            "unsupported_reference",
            "This HTTP gateway requires explicit conversation history instead of previous_response_id",
        ));
    }
    let model = body
        .get("model")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty() && s.len() <= 160)
        .ok_or_else(|| Error::invalid("A model name is required"))?
        .to_owned();
    if !body
        .get("input")
        .is_some_and(|v| v.is_array() || v.is_string())
    {
        return Err(Error::invalid("input must be a string or an array"));
    }
    if body.get("stream").is_some_and(|v| !v.is_boolean()) {
        return Err(Error::invalid("stream must be a boolean"));
    }
    if kind == RequestKind::Compact && body.get("stream").and_then(Value::as_bool) == Some(true) {
        return Err(Error::invalid(
            "Compact returns a single JSON response; streaming is not supported",
        ));
    }
    let sources = metadata(&body, headers)?;
    let compaction = crate::compaction::operation(&body, &sources, kind)?;
    let resolved = crate::identity_input::inspect(headers, &body, kind).resolved?;
    let stateless = resolved.is_none();
    let (session_id, thread_id) = resolved.map_or_else(
        || {
            let id = Uuid::new_v4().to_string();
            (id.clone(), id)
        },
        |resolved| (resolved.session_id, resolved.thread_id),
    );
    let turn_id = coherent(headers, None, &sources, "turn_id")?;
    for (field, header) in [
        ("window_id", Some("x-codex-window-id")),
        ("installation_id", Some("x-codex-installation-id")),
        ("parent_thread_id", Some("x-codex-parent-thread-id")),
        ("context_window_id", None),
        ("forked_from_thread_id", None),
        ("parent_turn_id", None),
        ("root_turn_id", None),
    ] {
        coherent(headers, header, &sources, field)?;
    }
    let requested_tier = body
        .get("service_tier")
        .filter(|v| !v.is_null())
        .map(|v| {
            v.as_str()
                .map(str::to_owned)
                .ok_or_else(|| Error::invalid("Invalid service_tier"))
        })
        .transpose()?;
    if requested_tier
        .as_deref()
        .is_some_and(|v| !["default", "auto", "fast", "priority"].contains(&v))
    {
        return Err(Error::invalid("Unsupported service_tier"));
    }
    let stream = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
    // Persist only the requested enum, never arbitrary reasoning content.
    let reasoning_effort = body
        .pointer("/reasoning/effort")
        .and_then(Value::as_str)
        .filter(|effort| {
            [
                "none", "minimal", "low", "medium", "high", "xhigh", "max", "ultra",
            ]
            .contains(effort)
        })
        .map(str::to_owned);
    let identifier_inputs = crate::identity_trace::client_identifiers(headers, &body);
    Ok(GatewayRequest {
        client_turn_state: xxgate_core::turn_state::TurnStateHeader::capture(headers),
        client_origin,
        client_metadata_present,
        identity_headers: crate::identity_trace::identity_headers(headers),
        stateless,
        ingress_diagnostics: None,
        kind,
        compaction,
        search_options: Default::default(),
        identity: ClientIdentity {
            session_id,
            thread_id,
            turn_id,
        },
        model,
        stream,
        requested_tier,
        reasoning_effort,
        identifier_inputs,
        document: ProtocolDocument {
            format: FORMAT,
            value: body,
        },
    })
}

impl IngressAdapter for ResponsesIngress {
    fn parse(&self, headers: &HeaderMap, body: Value) -> Result<GatewayRequest> {
        parse(headers, body, RequestKind::Responses)
    }
    fn encode(&self, doc: &ProtocolDocument) -> Result<Bytes> {
        if doc.format != FORMAT {
            return Err(Error::new(
                400,
                "unsupported_conversion",
                "The upstream event cannot be represented as Responses",
            ));
        }
        wire::frame(&doc.value)
    }
    fn heartbeat(&self, id: Uuid) -> Bytes {
        Bytes::from(format!(": xxgate.queue_heartbeat request_id={id}\n\n"))
    }
    fn failure(&self, id: Uuid, error: &Error) -> Bytes {
        wire::frame(&json!({"type":"response.failed","response":{"id":format!("resp_{}",id.simple()),"object":"response","status":"failed","error":{"type":"gateway_error","code":error.code,"message":error.client_message()},"usage":null,"output":[]}})).unwrap_or_default()
    }
    fn unary_response(&self, doc: &ProtocolDocument) -> Option<Value> {
        doc.value.get("response").cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reasoning_effort_is_recorded_without_retaining_arbitrary_content() {
        let headers = HeaderMap::from_iter([
            ("session-id".parse().unwrap(), "s".parse().unwrap()),
            ("thread-id".parse().unwrap(), "t".parse().unwrap()),
        ]);
        for (effort, expected) in [("high", Some("high")), ("PRIVATE_CONTENT", None)] {
            let parsed = ResponsesIngress
                .parse(
                    &headers,
                    json!({"model":"m","input":"hi","reasoning":{"effort":effort}}),
                )
                .unwrap();
            assert_eq!(parsed.reasoning_effort.as_deref(), expected);
            assert_eq!(parsed.document.value["reasoning"]["effort"], effort);
        }
    }
    #[test]
    fn metadata_conflicts_are_rejected() {
        let headers = HeaderMap::from_iter([
            ("session-id".parse().unwrap(), "a".parse().unwrap()),
            ("thread-id".parse().unwrap(), "t".parse().unwrap()),
        ]);
        assert_eq!(
            ResponsesIngress
                .parse(
                    &headers,
                    json!({"model":"m","input":"hi","client_metadata":{"session_id":"b"}})
                )
                .err()
                .unwrap()
                .code,
            "identity_conflict"
        );
        assert!(
            ResponsesIngress
                .parse(&headers, json!({"model":"m","input":"hi"}))
                .is_ok()
        );
        assert!(
            ResponsesIngress
                .parse(&HeaderMap::new(), json!({"model":"m","input":"hi"}))
                .unwrap()
                .stateless
        );
    }
    #[test]
    fn nested_parent_and_window_projections_must_agree() {
        let headers = HeaderMap::from_iter([
            ("session-id".parse().unwrap(), "s".parse().unwrap()),
            ("thread-id".parse().unwrap(), "t".parse().unwrap()),
            (
                "x-codex-parent-thread-id".parse().unwrap(),
                "p1".parse().unwrap(),
            ),
        ]);
        let body = json!({"model":"m","input":"hi","client_metadata":{"x-codex-turn-metadata":json!({"parent_thread_id":"p2"}).to_string()}});
        assert_eq!(
            ResponsesIngress.parse(&headers, body).err().unwrap().code,
            "identity_conflict"
        );
        let body = json!({"model":"m","input":"hi","client_metadata":{"x-codex-window-id":"w1","x-codex-turn-metadata":json!({"window_id":"w2"}).to_string()}});
        assert_eq!(
            ResponsesIngress.parse(&headers, body).err().unwrap().code,
            "identity_conflict"
        );
    }
}
