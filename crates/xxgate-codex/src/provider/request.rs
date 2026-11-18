use crate::wire::FORMAT;
use bytes::Bytes;
use http::{HeaderMap, HeaderValue, Method};
use serde_json::{Value, json};
use uuid::Uuid;
use xxgate_core::{
    Error, Result,
    accounts::{Account, Credentials},
    identity::IdentityMap,
    protocol::{GatewayRequest, PreparedRequest, RequestKind},
    providers::ModelSpec,
};

pub(crate) fn validate(r: &GatewayRequest, m: &ModelSpec) -> Result<()> {
    if r.kind.is_image() {
        return crate::images::validate(r, m);
    }
    if xxgate_core::providers::is_codex_image_model(&m.upstream) {
        return Err(Error::invalid(
            "Use /images/generations or /images/edits for this model",
        ));
    }
    if r.document.format != FORMAT && r.document.format != crate::search::FORMAT {
        return Err(Error::new(
            400,
            "unsupported_conversion",
            "This provider accepts Responses requests",
        ));
    }
    if matches!(
        r.requested_tier.as_deref(),
        Some("fast" | "priority" | "ultrafast")
    ) && !m.capabilities.fast
    {
        return Err(Error::invalid("Fast mode is not enabled for this model"));
    }
    if r.document
        .value
        .get("reasoning")
        .is_some_and(|v| !v.is_null())
        && !m.capabilities.reasoning
    {
        return Err(Error::invalid("Reasoning is not enabled for this model"));
    }
    if let Some(tools) = r.document.value.get("tools") {
        let tools = tools
            .as_array()
            .ok_or_else(|| Error::invalid("tools must be an array"))?;
        if !tools.is_empty() && !m.capabilities.tools {
            return Err(Error::invalid("Tools are not enabled for this model"));
        }
        if tools
            .iter()
            .any(|t| t.get("type").and_then(Value::as_str) == Some("image_generation"))
            && !m.capabilities.image_generation
        {
            return Err(Error::invalid(
                "Image generation is not enabled for this model",
            ));
        }
    }
    if !m.capabilities.image_input
        && has_image(r.document.value.get("input").unwrap_or(&Value::Null))
    {
        return Err(Error::invalid("Image input is not enabled for this model"));
    }
    Ok(())
}
fn has_image(value: &Value) -> bool {
    match value {
        Value::Array(a) => a.iter().any(has_image),
        Value::Object(o) => {
            o.get("type").and_then(Value::as_str) == Some("input_image")
                || o.get("content").is_some_and(has_image)
        }
        _ => false,
    }
}

pub(crate) fn auth_headers(a: &Account, c: &Credentials) -> Result<HeaderMap> {
    let mut headers = HeaderMap::new();
    for (key, value) in [
        ("authorization", format!("Bearer {}", c.access_token)),
        ("chatgpt-account-id", a.upstream_account_id.clone()),
        ("originator", "codex_cli_rs".into()),
        ("user-agent", a.profile.user_agent.clone()),
    ] {
        let mut value =
            HeaderValue::from_str(&value).map_err(|_| Error::invalid("Invalid account header"))?;
        if key == "authorization" {
            value.set_sensitive(true);
        }
        headers.insert(http::header::HeaderName::from_static(key), value);
    }
    Ok(headers)
}

pub(crate) fn prepare(
    r: &GatewayRequest,
    m: &ModelSpec,
    a: &Account,
    c: &Credentials,
    ids: &mut IdentityMap,
) -> Result<PreparedRequest> {
    prepare_source(r, m, a, c, ids, r.document.value.clone(), 0)
}

pub(crate) fn prepare_consuming(
    r: &mut GatewayRequest,
    m: &ModelSpec,
    a: &Account,
    c: &Credentials,
    ids: &mut IdentityMap,
    body_bytes: usize,
) -> Result<PreparedRequest> {
    let source = r.document.value.take();
    // Retry validation must still detect capability changes without retaining
    // conversation contents or large tool schemas through the response stream.
    r.document.value = retry_validation(&source);
    prepare_source(r, m, a, c, ids, source, body_bytes)
}

fn retry_validation(source: &Value) -> Value {
    let mut facts = serde_json::Map::new();
    if source.get("reasoning").is_some_and(|v| !v.is_null()) {
        facts.insert("reasoning".into(), json!({}));
    }
    if let Some(tools) = source.get("tools").and_then(Value::as_array) {
        let mut selected = Vec::new();
        if !tools.is_empty() {
            selected.push(json!({"type":"function"}));
        }
        if tools.iter().any(|t| t["type"] == "image_generation") {
            selected.push(json!({"type":"image_generation"}));
        }
        facts.insert("tools".into(), Value::Array(selected));
    }
    if has_image(&source["input"]) {
        facts.insert("input".into(), json!([{"type":"input_image"}]));
    }
    Value::Object(facts)
}

fn prepare_source(
    r: &GatewayRequest,
    m: &ModelSpec,
    a: &Account,
    c: &Credentials,
    ids: &mut IdentityMap,
    mut source: Value,
    body_bytes: usize,
) -> Result<PreparedRequest> {
    let (mut headers, client_meta) = normalized_headers_source(r, a, c, ids, &source)?;
    let trace_input = crate::identity_trace::input_identifiers(&source);
    let cache = source
        .get("prompt_cache_key")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    let mut body = serde_json::Map::new();
    // Codex OAuth rejects max_output_tokens even though the public Responses
    // API accepts it. Accept it at ingress for compatibility, but omit it here;
    // this transport cannot enforce the caller's requested output-token cap.
    for key in [
        "input",
        "instructions",
        "tools",
        "tool_choice",
        "parallel_tool_calls",
        "reasoning",
        "stream_options",
        "include",
        "text",
        "temperature",
        "top_p",
        "truncation",
        "access_programs",
    ] {
        if let Some(value) = source.as_object_mut().and_then(|s| s.remove(key)) {
            body.insert(key.into(), value);
        }
    }
    if r.websocket_session.is_some() {
        for key in ["previous_response_id", "generate"] {
            if let Some(value) = source.as_object_mut().and_then(|s| s.remove(key)) {
                body.insert(key.into(), value);
            }
        }
    }
    drop(source);
    // The Codex transport requires a message array and an instructions field,
    // even for ordinary Responses callers that do not provide Codex metadata.
    if let Some(input) = body.get_mut("input") {
        if input.is_string() {
            let text = input.take();
            *input = json!([{"role":"user","content":text}]);
        }
        if let Some(items) = input.as_array_mut() {
            for item in items {
                if item.get("role").and_then(Value::as_str) == Some("system") {
                    item["role"] = json!("developer");
                }
            }
        }
        super::references::rewrite_input(input, ids)?;
    }
    body.entry("instructions").or_insert_with(|| json!(""));
    if r.kind == RequestKind::Responses {
        super::search_history::ensure_declaration(&mut body);
    }
    body.insert("model".into(), json!(m.upstream.model));
    if r.kind != RequestKind::Compact {
        body.insert("store".into(), json!(false));
        body.insert("stream".into(), json!(true));
    }
    if let Some(tier) = r.requested_tier.as_deref().filter(|v| *v != "auto") {
        body.insert("service_tier".into(), json!(tier));
    }
    if r.kind == RequestKind::Compact {
        headers.insert("accept", HeaderValue::from_static("application/json"));
    }
    if let Some(client_meta) = client_meta {
        body.insert("client_metadata".into(), client_meta);
    }
    if let Some(cache) = cache.as_deref() {
        // Cache partitions are independent of session tracing. Preserve existing
        // Codex session-key mappings; otherwise scope a stable key by gateway Key
        // and account without creating a session or changing it on every request.
        let mapped =
            if !r.stateless && (cache == r.identity.session_id || cache == r.identity.thread_id) {
                ids.outbound("thread", cache)?
            } else {
                Uuid::new_v5(
                    &a.profile.installation_id,
                    format!("prompt_cache\0{}\0{cache}", ids.binding.session.key_id).as_bytes(),
                )
                .to_string()
            };
        if r.stateless && r.kind == RequestKind::Responses {
            // Match Codex OAuth cache-affinity behavior used by sub2api: an
            // explicitly supplied cache partition also supplies stable upstream
            // session headers. Reuse its already isolated value; do not turn it
            // into an internal conversation, binding, or client-source signal.
            insert_missing_header(&mut headers, "session_id", &mapped)?;
            insert_missing_header(&mut headers, "conversation_id", &mapped)?;
        }
        body.insert("prompt_cache_key".into(), json!(mapped));
    }
    let body = Value::Object(body);
    ids.record_request_rewrite(crate::identity_trace::compare_input_identifiers(
        &r.identifier_inputs,
        &trace_input,
        &headers,
        &body,
    ));
    let mut encoded = Vec::with_capacity(body_bytes.saturating_add(1024));
    serde_json::to_writer(&mut encoded, &body)
        .map_err(|_| Error::invalid("Invalid request body"))?;
    Ok(PreparedRequest {
        method: Method::POST,
        url: format!(
            "{}/responses{}",
            a.upstream_base_url.trim_end_matches('/'),
            if r.kind == RequestKind::Compact {
                "/compact"
            } else {
                ""
            }
        ),
        headers,
        body: Bytes::from(encoded.into_boxed_slice()),
        account_id: Some(a.id),
        profile_version: a.version,
        tls_backend: a.profile.tls_backend.clone(),
    })
}

fn identity_kind(name: &str) -> Option<&'static str> {
    match name {
        "session-id"
        | "session_id"
        | "conversation-id"
        | "conversation_id"
        | "thread-id"
        | "thread_id"
        | "parent_thread_id"
        | "forked_from_thread_id"
        | "x-codex-parent-thread-id" => Some("thread"),
        "turn_id" | "parent_turn_id" | "root_turn_id" | "x-codex-image-turn-id" => Some("turn"),
        "window_id" | "x-codex-window-id" => Some("window"),
        "context_window_id" => Some("context_window"),
        "x-client-request-id" => Some("request"),
        _ => None,
    }
}

fn mapped_identity(
    name: &str,
    value: &str,
    account: &Account,
    ids: &mut IdentityMap,
) -> Result<String> {
    if matches!(name, "installation_id" | "x-codex-installation-id") {
        return Ok(account.profile.installation_id.to_string());
    }
    match identity_kind(name) {
        Some(kind) => ids.outbound(kind, value.trim()),
        None => Ok(value.to_owned()),
    }
}

fn normalized_metadata(value: &Value, account: &Account, ids: &mut IdentityMap) -> Result<Value> {
    let mut value = value.clone();
    let object = value
        .as_object_mut()
        .ok_or_else(|| Error::invalid("Client metadata must be an object"))?;
    for (name, item) in object {
        if name == "x-codex-turn-metadata" {
            let nested: Value = serde_json::from_str(
                item.as_str()
                    .ok_or_else(|| Error::invalid("Invalid turn metadata"))?,
            )
            .map_err(|_| Error::invalid("Invalid turn metadata"))?;
            *item = json!(
                serde_json::to_string(&normalized_metadata(&nested, account, ids)?)
                    .map_err(|_| Error::invalid("Invalid turn metadata"))?
            );
        } else if (identity_kind(name).is_some()
            || matches!(name.as_str(), "installation_id" | "x-codex-installation-id"))
            && let Some(text) = item.as_str().filter(|s| !s.trim().is_empty())
        {
            *item = json!(mapped_identity(name, text, account, ids)?);
        }
    }
    Ok(value)
}

pub(crate) fn normalized_headers(
    r: &GatewayRequest,
    a: &Account,
    c: &Credentials,
    ids: &mut IdentityMap,
) -> Result<(HeaderMap, Option<Value>)> {
    normalized_headers_source(r, a, c, ids, &r.document.value)
}

fn normalized_headers_source(
    r: &GatewayRequest,
    a: &Account,
    c: &Credentials,
    ids: &mut IdentityMap,
    source: &Value,
) -> Result<(HeaderMap, Option<Value>)> {
    let mut headers = auth_headers(a, c)?;
    headers.insert("content-type", HeaderValue::from_static("application/json"));
    headers.insert("accept", HeaderValue::from_static("text/event-stream"));
    // Preserve explicit headers, then project identities supplied in the body
    // into the headers expected by the Codex transport.
    for (name, value) in &r.identity_headers {
        let text = value
            .to_str()
            .map_err(|_| Error::invalid("Invalid identity header"))?;
        if text.trim().is_empty() {
            continue;
        }
        let mapped = if name == "x-codex-turn-metadata" {
            let metadata: Value =
                serde_json::from_str(text).map_err(|_| Error::invalid("Invalid turn metadata"))?;
            serde_json::to_string(&normalized_metadata(&metadata, a, ids)?)
                .map_err(|_| Error::invalid("Invalid turn metadata"))?
        } else {
            mapped_identity(name.as_str(), text, a, ids)?
        };
        let canonical = match name.as_str() {
            "session_id" => "session-id",
            "thread_id" => "thread-id",
            "conversation_id" | "conversation-id" => "session-id",
            other => other,
        };
        // An explicit session header takes precedence over a conversation alias.
        if canonical == "session-id"
            && matches!(name.as_str(), "conversation_id" | "conversation-id")
            && ["session-id", "session_id"]
                .iter()
                .any(|n| r.identity_headers.contains_key(*n))
        {
            continue;
        }
        headers.insert(
            http::header::HeaderName::from_bytes(canonical.as_bytes())
                .map_err(|_| Error::invalid("Invalid header name"))?,
            HeaderValue::from_str(&mapped)
                .map_err(|_| Error::invalid("Invalid normalized header"))?,
        );
    }
    let metadata = if r.client_metadata_present {
        Some(normalized_metadata(&source["client_metadata"], a, ids)?)
    } else {
        None
    };
    if !r.stateless {
        let session = ids.outbound("thread", &r.identity.session_id)?;
        let thread = ids.outbound("thread", &r.identity.thread_id)?;
        insert_missing_header(&mut headers, "session-id", &session)?;
        insert_missing_header(&mut headers, "thread-id", &thread)?;
        insert_missing_header(&mut headers, "x-client-request-id", &thread)?;
    }

    // Reuse the same validated sources and mappings as the body. Only project
    // known identity fields; arbitrary client metadata must not become headers.
    let sources = crate::ingress::metadata(source, &r.identity_headers)?;
    let mut turn = serde_json::Map::new();
    if !r.stateless {
        turn.insert(
            "session_id".into(),
            json!(ids.outbound("thread", &r.identity.session_id)?),
        );
        turn.insert(
            "thread_id".into(),
            json!(ids.outbound("thread", &r.identity.thread_id)?),
        );
    }
    for (field, header) in [
        ("installation_id", Some("x-codex-installation-id")),
        ("window_id", Some("x-codex-window-id")),
        ("parent_thread_id", Some("x-codex-parent-thread-id")),
        ("turn_id", None),
        ("context_window_id", None),
        ("forked_from_thread_id", None),
        ("parent_turn_id", None),
        ("root_turn_id", None),
    ] {
        let value = if field == "turn_id" {
            let prewarm = r.websocket_session.is_some()
                && source.get("generate").and_then(Value::as_bool) == Some(false);
            crate::ingress::turn_id(&sources, prewarm)?
        } else {
            crate::ingress::coherent(&r.identity_headers, header, &sources, field)?
        };
        if let Some(value) = value {
            // Only the prewarm turn may be empty; it has no identity to map.
            let mapped = if field == "turn_id" && value.is_empty() {
                value
            } else {
                mapped_identity(field, &value, a, ids)?
            };
            if let Some(header) = header {
                insert_missing_header(&mut headers, header, &mapped)?;
            }
            turn.insert(field.into(), json!(mapped));
        }
    }
    turn.extend(crate::compaction::turn_metadata(&sources)?);
    if !turn.is_empty() {
        if let Some(existing) = headers.get("x-codex-turn-metadata") {
            let mut projected: Value = serde_json::from_slice(existing.as_bytes())
                .map_err(|_| Error::invalid("Invalid turn metadata"))?;
            let object = projected
                .as_object_mut()
                .ok_or_else(|| Error::invalid("Invalid turn metadata"))?;
            for (key, value) in turn {
                if key == "compaction"
                    && let Some(previous) = object.get_mut(&key).and_then(Value::as_object_mut)
                    && let Some(fields) = value.as_object()
                {
                    for (field, value) in fields {
                        previous
                            .entry(field.clone())
                            .or_insert_with(|| value.clone());
                    }
                    continue;
                }
                object.entry(key).or_insert(value);
            }
            headers.insert(
                "x-codex-turn-metadata",
                HeaderValue::from_str(&projected.to_string())
                    .map_err(|_| Error::invalid("Invalid turn metadata"))?,
            );
        } else {
            let value = serde_json::to_string(&turn)
                .map_err(|_| Error::invalid("Invalid turn metadata"))?;
            insert_missing_header(&mut headers, "x-codex-turn-metadata", &value)?;
        }
    }
    Ok((headers, metadata))
}

fn insert_missing_header(headers: &mut HeaderMap, name: &'static str, value: &str) -> Result<()> {
    if !headers.contains_key(name) {
        headers.insert(
            name,
            HeaderValue::from_str(value)
                .map_err(|_| Error::invalid("Invalid normalized header"))?,
        );
    }
    Ok(())
}

#[cfg(test)]
mod memory_tests {
    use super::*;
    use xxgate_core::{
        identity::{Binding, SessionKey},
        protocol::IngressAdapter,
        types::ModelRef,
    };

    fn fixture() -> (Account, Credentials, ModelSpec, IdentityMap) {
        let id = Uuid::new_v4();
        let a:Account=serde_json::from_value(json!({"id":id,"name":"test","provider":"openai","access_kind":"codex_oauth","enabled":true,"disable_reason":null,"max_inflight":1,"upstream_account_id":"test","upstream_base_url":"https://example.invalid","models":[],"profile":{"installation_id":Uuid::new_v4(),"codex_version":"test","user_agent":"test","tls_backend":"rustls"},"version":1,"credential_version":1,"credential_expires_at":null,"created_at":"2026-11-09T00:00:00Z","updated_at":"2026-11-09T00:00:00Z"})).unwrap();
        let c = serde_json::from_value(
            json!({"access_token":"synthetic","refresh_token":"","expires_at":null}),
        )
        .unwrap();
        let m=serde_json::from_value(json!({"id":"test","upstream":{"provider":"openai","access_kind":"codex_oauth","model":"test"},"enabled":true,"capabilities":{},"version":1})).unwrap();
        let ids = IdentityMap::new(
            Binding::new(
                SessionKey {
                    key_id: Uuid::new_v4(),
                    client_session_id: "s".into(),
                },
                id,
                &ModelRef::codex("test"),
                1,
            ),
            vec![],
        );
        (a, c, m, ids)
    }

    #[test]
    fn fast_tiers_require_model_capability_and_preserve_the_wire_value() {
        let (a, c, mut m, mut ids) = fixture();
        for tier in ["fast", "priority", "ultrafast"] {
            let mut request = crate::ingress::ResponsesIngress
                .parse(
                    &HeaderMap::new(),
                    json!({"model":"test","input":[],"service_tier":tier}),
                )
                .unwrap();
            m.capabilities.fast = false;
            assert!(validate(&request, &m).is_err());
            m.capabilities.fast = true;
            validate(&request, &m).unwrap();
            let prepared = prepare(&request, &m, &a, &c, &mut ids).unwrap();
            let body: Value = serde_json::from_slice(&prepared.body).unwrap();
            assert_eq!(body["service_tier"], tier);
            let prepared = prepare_consuming(&mut request, &m, &a, &c, &mut ids, 4096).unwrap();
            let body: Value = serde_json::from_slice(&prepared.body).unwrap();
            assert_eq!(body["service_tier"], tier);
            m.capabilities.fast = false;
            assert!(validate(&request, &m).is_err());
        }
    }

    #[test]
    fn consuming_prepare_preserves_wire_and_retry_requirements_without_retaining_payload() {
        let (a, c, m, mut ids) = fixture();
        let body = json!({"model":"test","input":[{"role":"user","content":[{"type":"input_text","text":"PRIVATE_TEXT".repeat(10000)},{"type":"input_image","image_url":"PRIVATE_IMAGE".repeat(10000)}]}],
            "instructions":"PRIVATE_INSTRUCTIONS".repeat(1000),"tools":[{"type":"image_generation"},{"type":"function","name":"f","parameters":{"description":"PRIVATE_SCHEMA".repeat(10000)}}],"reasoning":{"effort":"high"},"service_tier":"fast"});
        let bytes = serde_json::to_vec(&body).unwrap().len();
        let mut r = crate::ingress::ResponsesIngress
            .parse(&HeaderMap::new(), body)
            .unwrap();
        validate(&r, &m).unwrap();
        let expected = prepare(&r, &m, &a, &c, &mut ids).unwrap();
        let actual = prepare_consuming(&mut r, &m, &a, &c, &mut ids, bytes).unwrap();
        assert_eq!(actual.body, expected.body);
        assert_eq!(actual.headers, expected.headers);
        assert!(r.document.value.to_string().len() < 256);
        assert!(!r.document.value.to_string().contains("PRIVATE"));
        for capability in ["image", "image_tool", "tools", "reasoning", "fast"] {
            let mut changed = m.clone();
            match capability {
                "image" => changed.capabilities.image_input = false,
                "image_tool" => changed.capabilities.image_generation = false,
                "tools" => changed.capabilities.tools = false,
                "reasoning" => changed.capabilities.reasoning = false,
                _ => changed.capabilities.fast = false,
            }
            assert!(
                validate(&r, &changed).is_err(),
                "{capability} must still be checked during recovery"
            );
        }
    }
    #[test]
    fn consuming_native_prewarm_keeps_empty_turn_and_generate_false() {
        let (a, c, m, mut ids) = fixture();
        let mut r=crate::ingress::parse_websocket(&HeaderMap::new(),json!({"type":"response.create","model":"test","generate":false,"input":"hello","client_metadata":{"turn_id":""}}),Uuid::new_v4()).unwrap();
        let prepared = prepare_consuming(&mut r, &m, &a, &c, &mut ids, 1024).unwrap();
        let body: Value = serde_json::from_slice(&prepared.body).unwrap();
        assert_eq!(body["generate"], false);
        assert_eq!(body["client_metadata"]["turn_id"], "");
        assert_eq!(body["input"][0]["content"], "hello");
    }
}
