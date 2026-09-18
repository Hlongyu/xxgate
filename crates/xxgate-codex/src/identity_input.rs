//! Resolve explicit identity signals and retain bounded, content-free diagnostics.
use http::HeaderMap;
use serde::Serialize;
use serde_json::{Value, json};
use xxgate_core::{Error, Result, protocol::RequestKind};

#[derive(Clone, Serialize)]
pub struct ResolvedIdentity {
    pub session_id: String,
    pub thread_id: String,
    pub session_sources: Vec<String>,
    pub thread_sources: Vec<String>,
}

#[derive(Serialize)]
struct Candidate {
    field: &'static str,
    source: String,
    status: &'static str,
    value: Option<String>,
    bytes: usize,
}

pub struct Inspection {
    pub resolved: Result<Option<ResolvedIdentity>>,
    pub report: Value,
}

fn candidate(field: &'static str, source: String, value: &Value) -> Candidate {
    let (status, text, bytes) = match value {
        Value::Null => ("empty", None, 0),
        Value::String(s) if s.len() > 512 => ("too_long", None, s.len()),
        Value::String(s) if s.chars().any(char::is_control) => {
            ("invalid_characters", None, s.len())
        }
        Value::String(s) if s.trim().is_empty() => ("empty", None, s.len()),
        Value::String(s) => ("valid", Some(s.trim().to_owned()), s.len()),
        _ => ("invalid_type", None, 0),
    };
    Candidate {
        field,
        source,
        status,
        value: text,
        bytes,
    }
}

fn header_candidates(
    out: &mut Vec<Candidate>,
    headers: &HeaderMap,
    field: &'static str,
    names: &[&str],
) -> Result<()> {
    for name in names {
        for (index, value) in headers.get_all(*name).iter().enumerate() {
            if index >= 4 {
                return Err(Error::invalid(format!("Too many occurrences of {name}")));
            }
            let source = format!("headers.{name}[{index}]");
            match value.to_str() {
                Ok(value) => out.push(candidate(field, source, &json!(value))),
                Err(_) => out.push(Candidate {
                    field,
                    source,
                    status: "invalid_encoding",
                    value: None,
                    bytes: value.as_bytes().len(),
                }),
            }
        }
    }
    Ok(())
}

fn metadata_candidates(out: &mut Vec<Candidate>, value: &Value, source: &str) -> Result<()> {
    if !value.is_object() {
        return Err(Error::invalid(format!("{source} must be a JSON object")));
    }
    for (field, names) in [
        ("session_id", ["session_id", "session-id"]),
        ("thread_id", ["thread_id", "thread-id"]),
        ("conversation_id", ["conversation_id", "conversation-id"]),
    ] {
        for name in names {
            if let Some(value) = value.get(name) {
                out.push(candidate(field, format!("{source}.{name}"), value));
            }
        }
    }
    for field in [
        "turn_id",
        "parent_thread_id",
        "forked_from_thread_id",
        "parent_turn_id",
        "root_turn_id",
        "window_id",
        "context_window_id",
        "installation_id",
        "x-codex-window-id",
        "x-codex-installation-id",
        "x-codex-parent-thread-id",
    ] {
        if let Some(value) = value.get(field) {
            out.push(candidate(field, format!("{source}.{field}"), value));
        }
    }
    Ok(())
}

fn parse_metadata(text: &str, source: &str) -> Result<Value> {
    if text.len() > 128 * 1024 {
        return Err(Error::invalid(format!("{source} exceeds 128 KiB")));
    }
    serde_json::from_str(text).map_err(|_| Error::invalid(format!("{source} is not valid JSON")))
}

fn collect(
    out: &mut Vec<Candidate>,
    headers: &HeaderMap,
    body: &Value,
    kind: RequestKind,
) -> Result<()> {
    for (field, names) in [
        ("session_id", ["session-id", "session_id"]),
        ("thread_id", ["thread-id", "thread_id"]),
        ("conversation_id", ["conversation-id", "conversation_id"]),
    ] {
        header_candidates(out, headers, field, &names)?;
    }
    for (field, name) in [
        ("window_id", "x-codex-window-id"),
        ("installation_id", "x-codex-installation-id"),
        ("parent_thread_id", "x-codex-parent-thread-id"),
    ] {
        header_candidates(out, headers, field, &[name])?;
    }
    if let Some(meta) = body.get("client_metadata") {
        metadata_candidates(out, meta, "body.client_metadata")?;
        if let Some(nested) = meta.get("x-codex-turn-metadata") {
            let source = "body.client_metadata.x-codex-turn-metadata";
            let text = nested
                .as_str()
                .ok_or_else(|| Error::invalid(format!("{source} must be a JSON string")))?;
            metadata_candidates(out, &parse_metadata(text, source)?, source)?;
        }
    }
    if let Some(meta) = headers.get("x-codex-turn-metadata") {
        let source = "headers.x-codex-turn-metadata";
        if headers.get_all("x-codex-turn-metadata").iter().count() != 1 {
            return Err(Error::invalid("Duplicate x-codex-turn-metadata headers"));
        }
        let text = meta
            .to_str()
            .map_err(|_| Error::invalid("Invalid x-codex-turn-metadata encoding"))?;
        metadata_candidates(out, &parse_metadata(text, source)?, source)?;
    }
    if let Some(value) = body.get("prompt_cache_key") {
        out.push(candidate(
            "prompt_cache_key",
            "body.prompt_cache_key".into(),
            value,
        ));
    }
    if kind == RequestKind::Search
        && let Some(value) = body.get("id")
    {
        out.push(candidate("search_id", "body.id".into(), value));
    }
    Ok(())
}

type Signal = Option<(String, Vec<String>)>;
fn coherent(candidates: &[Candidate], field: &str) -> Result<Signal> {
    let values: Vec<_> = candidates.iter().filter(|c| c.field == field).collect();
    if let Some(invalid) = values
        .iter()
        .find(|c| !["valid", "empty"].contains(&c.status))
    {
        return Err(Error::invalid(format!(
            "Invalid identity at {} ({})",
            invalid.source, invalid.status
        )));
    }
    let valid: Vec<_> = values.into_iter().filter(|c| c.value.is_some()).collect();
    let Some(first) = valid.first() else {
        return Ok(None);
    };
    if valid.iter().any(|c| c.value != first.value) {
        return Err(Error::new(
            400,
            "identity_conflict",
            format!(
                "Conflicting {field} values in {}",
                valid
                    .iter()
                    .map(|c| c.source.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        ));
    }
    Ok(Some((
        first.value.clone().expect("filtered value"),
        valid.iter().map(|c| c.source.clone()).collect(),
    )))
}

pub fn inspect(headers: &HeaderMap, body: &Value, kind: RequestKind) -> Inspection {
    let mut fields = vec![];
    let resolved = collect(&mut fields, headers, body, kind).and_then(|()| {
        let session = coherent(&fields, "session_id")?;
        let thread = coherent(&fields, "thread_id")?;
        let conversation = coherent(&fields, "conversation_id")?;
        let _cache = coherent(&fields, "prompt_cache_key")?;
        let search = coherent(&fields, "search_id")?;
        let Some((session_id, session_sources)) = session
            .or_else(|| conversation.clone())
            .or_else(|| thread.clone())
            .or(search)
        else {
            return Ok(None);
        };
        let (thread_id, thread_sources) = thread
            .or(conversation)
            .unwrap_or_else(|| (session_id.clone(), vec!["resolved.session_id".into()]));
        Ok(Some(ResolvedIdentity {
            session_id,
            thread_id,
            session_sources,
            thread_sources,
        }))
    });
    let report = match &resolved {
        Ok(None) => json!({"status":"stateless","fields":fields}),
        Ok(Some(resolved)) => json!({"status":"resolved","fields":fields,"resolved":resolved}),
        Err(error) => {
            json!({"status":"failed","fields":fields,"error":{"code":error.code,"message":error.message}})
        }
    };
    Inspection { resolved, report }
}

pub fn diagnostics(
    method: &str,
    path: &str,
    headers: &HeaderMap,
    body: Option<&Value>,
    kind: RequestKind,
) -> Value {
    let inspected = inspect(headers, body.unwrap_or(&Value::Null), kind);
    let origin = crate::client_source::detect(headers, body.unwrap_or(&Value::Null));
    let mut selected = serde_json::Map::new();
    for name in [
        "content-type",
        "content-encoding",
        "user-agent",
        "originator",
        "version",
        "x-request-id",
        "x-client-request-id",
        "traceparent",
    ] {
        if let Some(value) = headers.get(name) {
            selected.insert(name.into(),json!({"value":value.to_str().ok().filter(|s|s.len()<=512&&!s.chars().any(char::is_control)),"bytes":value.as_bytes().len()}));
        }
    }
    let mut shape = serde_json::Map::new();
    if let Some(body) = body {
        for name in [
            "model",
            "input",
            "stream",
            "client_metadata",
            "metadata",
            "prompt_cache_key",
            "previous_response_id",
            "background",
            "tools",
            "reasoning",
            "commands",
            "id",
        ] {
            if let Some(value) = body.get(name) {
                shape.insert(
                    name.into(),
                    match value {
                        Value::Null => json!("null"),
                        Value::Bool(_) => json!("boolean"),
                        Value::Number(_) => json!("number"),
                        Value::String(s) => json!({"type":"string","bytes":s.len()}),
                        Value::Array(a) => json!({"type":"array","length":a.len()}),
                        Value::Object(_) => json!("object"),
                    },
                );
            }
        }
    }
    json!({"client_origin":origin,"method":method,"path":path,"headers":selected,"authorization_present":headers.contains_key("authorization"),"body_status":if body.is_some(){"parsed"}else{"not_parsed"},"body_fields":shape,"identity":if body.is_some(){inspected.report}else{json!({"status":"body_not_parsed","fields":inspected.report["fields"]})}})
}

#[cfg(test)]
#[path = "identity_input_tests.rs"]
mod tests;
