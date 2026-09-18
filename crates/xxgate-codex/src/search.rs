use bytes::Bytes;
use http::{HeaderMap, HeaderValue, Method};
use serde_json::{Value, json};
use xxgate_core::{
    Error, Result,
    accounts::{Account, Credentials},
    identity::{IdentifierRewrite, IdentityMap},
    protocol::{GatewayRequest, IngressAdapter, PreparedRequest, RequestKind, SearchOptions},
    providers::ModelSpec,
};

pub const FORMAT: &str = "codex.search.v1";

pub fn parse(headers: &HeaderMap, body: Value, query: Option<&str>) -> Result<GatewayRequest> {
    if !body.is_object() {
        return Err(Error::invalid("Search request must be a JSON object"));
    }
    body.get("id")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty() && s.len() <= 512 && !s.chars().any(char::is_control))
        .ok_or_else(|| Error::invalid("Search requires a nonempty id of at most 512 bytes"))?;
    if body.get("stream").is_some_and(|v| v != &json!(false)) {
        return Err(Error::invalid(
            "Search returns a single JSON response; streaming is not supported",
        ));
    }
    if query.is_some_and(|q| q.len() > 8192) {
        return Err(Error::invalid("Search query string is too long"));
    }
    if headers.get_all("openai-beta").iter().count() > 1 {
        return Err(Error::invalid("Duplicate OpenAI-Beta headers"));
    }
    let resolved = crate::identity_input::inspect(headers, &body, RequestKind::Search)
        .resolved?
        .ok_or_else(|| Error::invalid("Search identity is missing"))?;
    let session = resolved.session_id;
    let thread = resolved.thread_id;
    let mut effective_headers = headers.clone();
    for (name, value) in [("session-id", session), ("thread-id", thread)] {
        effective_headers.insert(
            name,
            HeaderValue::from_str(&value).map_err(|_| Error::invalid("Invalid search identity"))?,
        );
    }
    // Reuse identity conflict checks without imposing the Responses body schema
    // on the evolving Search commands and opaque context.
    let mut projection = json!({"input":""});
    for field in [
        "model",
        "input",
        "reasoning",
        "service_tier",
        "client_metadata",
        "prompt_cache_key",
    ] {
        if let Some(value) = body.get(field) {
            projection[field] = value.clone();
        }
    }
    let mut request = crate::ingress::ResponsesIngress.parse(&effective_headers, projection)?;
    request.kind = RequestKind::Search;
    request.compaction = None;
    request.client_origin = crate::client_source::detect(headers, &body);
    request.client_metadata_present = body.get("client_metadata").is_some();
    request.search_options = SearchOptions {
        query: query.map(str::to_owned),
        beta: headers.get("openai-beta").cloned(),
        client_metadata_present: body.get("client_metadata").is_some(),
    };
    request.identifier_inputs = crate::identity_trace::client_identifiers(headers, &body);
    request.identity_headers = crate::identity_trace::identity_headers(headers);
    request.document.value = body;
    request.document.format = FORMAT;
    Ok(request)
}

pub(crate) fn prepare(
    r: &GatewayRequest,
    model: &ModelSpec,
    account: &Account,
    credentials: &Credentials,
    ids: &mut IdentityMap,
) -> Result<PreparedRequest> {
    let mut body = r.document.value.clone();
    body["model"] = json!(model.upstream.model);
    let (mut headers, metadata) =
        crate::provider::request::normalized_headers(r, account, credentials, ids)?;
    if let Some(metadata) = metadata {
        body["client_metadata"] = metadata;
    } else {
        body.as_object_mut()
            .ok_or_else(|| Error::invalid("Invalid search body"))?
            .remove("client_metadata");
    }
    // Search IDs and input identifiers stay bound to opaque encrypted content.
    headers.insert("accept", HeaderValue::from_static("application/json"));
    headers.insert(
        "version",
        HeaderValue::from_str(&account.profile.codex_version)
            .map_err(|_| Error::invalid("Invalid client version"))?,
    );
    if let Some(beta) = &r.search_options.beta {
        headers.insert("openai-beta", beta.clone());
    }
    let mut rewrite =
        crate::identity_trace::compare(&r.identifier_inputs, &r.document.value, &headers, &body);
    let search_id = body.get("id").and_then(Value::as_str).map(str::to_owned);
    rewrite.entries.push(IdentifierRewrite {
        field: "body.id".into(),
        before: search_id.clone(),
        after: search_id,
        action: "unchanged".into(),
    });
    ids.record_request_rewrite(rewrite);
    let mut url = format!(
        "{}/alpha/search",
        account.upstream_base_url.trim_end_matches('/')
    );
    if let Some(query) = &r.search_options.query {
        url.push('?');
        url.push_str(query);
    }
    Ok(PreparedRequest {
        method: Method::POST,
        url,
        headers,
        body: Bytes::from(
            serde_json::to_vec(&body).map_err(|_| Error::invalid("Invalid search body"))?,
        ),
        account_id: Some(account.id),
        profile_version: account.version,
        tls_backend: account.profile.tls_backend.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn search_identity_falls_back_to_id_and_keeps_opaque_commands() {
        let body = json!({"id":"opaque-search-session","model":"m","commands":{"search_query":[{"q":"PRIVATE_QUERY"}]},"future":true});
        let request = parse(&HeaderMap::new(), body.clone(), Some("feature=1")).unwrap();
        assert_eq!(request.kind, RequestKind::Search);
        assert!(!request.stream);
        assert_eq!(request.identity.session_id, "opaque-search-session");
        assert_eq!(request.identity.thread_id, "opaque-search-session");
        assert_eq!(request.document.value["commands"], body["commands"]);
        assert_eq!(request.document.value["future"], true);
        assert_eq!(request.search_options.query.as_deref(), Some("feature=1"));
        let headers = HeaderMap::from_iter([
            ("session-id".parse().unwrap(), "session".parse().unwrap()),
            ("thread-id".parse().unwrap(), "thread".parse().unwrap()),
        ]);
        let request = parse(&headers, body.clone(), None).unwrap();
        assert_eq!(request.identity.session_id, "session");
        assert_eq!(request.identity.thread_id, "thread");
        let mut bad = body.clone();
        bad["client_metadata"] = json!({"session_id":"other"});
        assert_eq!(
            parse(&headers, bad, None).err().unwrap().code,
            "identity_conflict"
        );
        for bad in [
            json!({"model":"m"}),
            json!({"id":"s","model":"m","stream":true}),
            json!({"id":"s","model":"m","input":7}),
        ] {
            assert!(parse(&HeaderMap::new(), bad, None).is_err());
        }
    }
}
