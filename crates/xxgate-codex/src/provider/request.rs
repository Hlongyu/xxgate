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
    if r.document.format != FORMAT && r.document.format != crate::search::FORMAT {
        return Err(Error::new(
            400,
            "unsupported_conversion",
            "This provider accepts Responses requests",
        ));
    }
    if matches!(r.requested_tier.as_deref(), Some("fast" | "priority")) && !m.capabilities.fast {
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
    let source = &r.document.value;
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
        if let Some(value) = source.get(key) {
            body.insert(key.into(), value.clone());
        }
    }
    // The Codex transport requires a message array and an instructions field,
    // even for ordinary Responses callers that do not provide Codex metadata.
    if let Some(input) = body.get_mut("input") {
        if let Some(text) = input.as_str() {
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
    body.insert("model".into(), json!(m.upstream.model));
    if r.kind != RequestKind::Compact {
        body.insert("store".into(), json!(false));
        body.insert("stream".into(), json!(true));
    }
    if let Some(tier) = r.requested_tier.as_deref().filter(|v| *v != "auto") {
        body.insert("service_tier".into(), json!(tier));
    }
    let (mut headers, client_meta) = normalized_headers(r, a, c, ids)?;
    if r.kind == RequestKind::Compact {
        headers.insert("accept", HeaderValue::from_static("application/json"));
    }
    if let Some(client_meta) = client_meta {
        body.insert("client_metadata".into(), client_meta);
    }
    if let Some(cache) = source
        .get("prompt_cache_key")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
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
    ids.record_request_rewrite(crate::identity_trace::compare(
        &r.identifier_inputs,
        source,
        &headers,
        &body,
    ));
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
        body: Bytes::from(
            serde_json::to_vec(&body).map_err(|_| Error::invalid("Invalid request body"))?,
        ),
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
        "turn_id" | "parent_turn_id" | "root_turn_id" => Some("turn"),
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
        Some(normalized_metadata(
            &r.document.value["client_metadata"],
            a,
            ids,
        )?)
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
    let sources = crate::ingress::metadata(&r.document.value, &r.identity_headers)?;
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
        if let Some(value) = crate::ingress::coherent(&r.identity_headers, header, &sources, field)?
        {
            let mapped = mapped_identity(field, &value, a, ids)?;
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
