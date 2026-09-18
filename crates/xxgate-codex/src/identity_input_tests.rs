use super::*;
fn headers(values: &[(&str, &str)]) -> HeaderMap {
    let mut h = HeaderMap::new();
    for (k, v) in values {
        h.append(
            k.parse::<http::header::HeaderName>().unwrap(),
            v.parse().unwrap(),
        );
    }
    h
}

#[test]
fn aliases_and_fallbacks_preserve_stable_session_and_thread_identity() {
    for h in [
        headers(&[("session_id", "s")]),
        headers(&[("session-id", "s")]),
        headers(&[("conversation_id", "s")]),
        headers(&[("conversation-id", "s")]),
        headers(&[("thread_id", "s")]),
        headers(&[("thread-id", "s")]),
    ] {
        let resolved = inspect(&h, &json!({}), RequestKind::Responses)
            .resolved
            .unwrap()
            .unwrap();
        assert_eq!(resolved.session_id, "s");
        assert_eq!(resolved.thread_id, "s");
    }
    for body in [
        json!({"client_metadata":{"session-id":"s"}}),
        json!({"client_metadata":{"conversation_id":"s"}}),
        json!({"client_metadata":{"x-codex-turn-metadata":"{\"session_id\":\"s\"}"}}),
    ] {
        assert_eq!(
            inspect(&HeaderMap::new(), &body, RequestKind::Responses)
                .resolved
                .unwrap()
                .unwrap()
                .session_id,
            "s"
        );
    }
    let h = headers(&[
        ("session-id", "root"),
        ("session_id", " root "),
        ("conversation_id", "conversation"),
        ("thread_id", "child"),
    ]);
    let resolved = inspect(
        &h,
        &json!({"prompt_cache_key":"cache"}),
        RequestKind::Responses,
    )
    .resolved
    .unwrap()
    .unwrap();
    assert_eq!(resolved.session_id, "root");
    assert_eq!(resolved.thread_id, "child");
    let h = headers(&[("session-id", " "), ("thread_id", "thread")]);
    assert_eq!(
        inspect(
            &h,
            &json!({"prompt_cache_key":"shared-cache"}),
            RequestKind::Responses
        )
        .resolved
        .unwrap()
        .unwrap()
        .session_id,
        "thread"
    );
}

#[test]
fn conflicts_and_invalid_metadata_have_bounded_diagnostics_without_content() {
    let h = headers(&[
        ("session-id", "a"),
        ("session_id", "b"),
        ("authorization", "Bearer PRIVATE_TOKEN"),
        ("cookie", "PRIVATE_COOKIE"),
        ("x-api-key", "PRIVATE_KEY"),
    ]);
    let body = json!({"input":"PRIVATE_PROMPT","client_metadata":{"notes":"PRIVATE_NOTE"},"tools":[{"description":"PRIVATE_TOOL"}],"reasoning":{"encrypted_content":"PRIVATE_CIPHER"}});
    let inspection = inspect(&h, &body, RequestKind::Responses);
    assert_eq!(inspection.resolved.err().unwrap().code, "identity_conflict");
    assert_eq!(inspection.report["fields"][0]["value"], "a");
    assert_eq!(inspection.report["fields"][1]["value"], "b");
    let report = diagnostics(
        "POST",
        "/v1/responses",
        &h,
        Some(&body),
        RequestKind::Responses,
    );
    assert!(!report.to_string().contains("PRIVATE_"));
    assert_eq!(report["authorization_present"], true);
    let report = inspect(
        &HeaderMap::new(),
        &json!({"prompt_cache_key":"PRIVATE_".repeat(1000)}),
        RequestKind::Responses,
    )
    .report;
    assert_eq!(report["fields"][0]["status"], "too_long");
    assert!(report.to_string().len() < 1024);
    assert!(!report.to_string().contains("PRIVATE_"));
    let inspection = inspect(
        &HeaderMap::new(),
        &json!({"client_metadata":{"x-codex-turn-metadata":"PRIVATE_INVALID_JSON"}}),
        RequestKind::Responses,
    );
    assert!(inspection.resolved.is_err());
    assert!(!inspection.report.to_string().contains("PRIVATE_"));
}

#[test]
fn missing_identity_is_stateless_but_invalid_supplied_identity_still_fails() {
    let inspection = inspect(&HeaderMap::new(), &json!({}), RequestKind::Responses);
    assert!(inspection.resolved.unwrap().is_none());
    assert_eq!(inspection.report["status"], "stateless");
    assert!(inspection.report.get("error").is_none());
    for body in [
        json!({"prompt_cache_key": 1}),
        json!({"client_metadata":{"session_id":"x".repeat(513)}}),
    ] {
        assert!(
            inspect(&HeaderMap::new(), &body, RequestKind::Responses)
                .resolved
                .is_err()
        );
    }
    use xxgate_core::protocol::IngressAdapter;
    let body = json!({"model":"gpt-5.5","input":"ordinary Responses input"});
    let first = crate::ingress::ResponsesIngress
        .parse(&HeaderMap::new(), body.clone())
        .unwrap();
    let second = crate::ingress::ResponsesIngress
        .parse(&HeaderMap::new(), body)
        .unwrap();
    assert!(first.stateless && second.stateless);
    assert_ne!(first.identity.session_id, second.identity.session_id);
    assert_eq!(first.identity.session_id, first.identity.thread_id);
}

#[test]
fn cache_partition_is_not_a_client_session() {
    let result = inspect(
        &HeaderMap::new(),
        &json!({"prompt_cache_key":"shared-cache"}),
        RequestKind::Responses,
    );
    assert!(result.resolved.unwrap().is_none());
    assert_eq!(result.report["status"], "stateless");
    assert_eq!(result.report["fields"][0]["field"], "prompt_cache_key");
}
