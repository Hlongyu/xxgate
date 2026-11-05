use super::*;

fn websocket_body(turn: Value) -> Value {
    json!({
        "type":"response.create", "model":"test-model", "input":[],
        "client_metadata":{
            "turn_id":turn,
            "x-codex-turn-metadata":json!({"turn_id":turn}).to_string()
        }
    })
}

#[test]
fn only_websocket_prewarm_accepts_an_empty_turn() {
    let headers = HeaderMap::new();
    let session = Uuid::new_v4();
    let mut body = websocket_body(json!(""));
    for generate in [
        None,
        Some(json!(true)),
        Some(json!("false")),
        Some(Value::Null),
    ] {
        if let Some(generate) = generate {
            body["generate"] = generate;
        }
        assert!(parse_websocket(&headers, body.clone(), session).is_err());
    }
    body["generate"] = json!(false);
    let parsed = parse_websocket(&headers, body.clone(), session).unwrap();
    assert!(parsed.identity.turn_id.is_none());
    assert_eq!(parsed.websocket_session, Some(session));
    assert_eq!(parsed.document.value["generate"], false);
    assert_eq!(parsed.document.value["client_metadata"]["turn_id"], "");
    assert!(!parsed.stateless);

    body.as_object_mut().unwrap().remove("type");
    assert!(ResponsesIngress.parse(&headers, body.clone()).is_err());
    body.as_object_mut().unwrap().remove("generate");
    assert!(ResponsesIngress.parse(&headers, body).is_err());
}

#[test]
fn prewarm_still_rejects_invalid_and_conflicting_metadata() {
    for turn in [
        json!("x".repeat(513)),
        json!("中".repeat(171)),
        json!("a\nb"),
        json!("\t"),
        json!(12),
    ] {
        let mut body = websocket_body(turn);
        body["generate"] = json!(false);
        assert!(parse_websocket(&HeaderMap::new(), body, Uuid::new_v4()).is_err());
    }
    for nested in [json!("different-turn"), json!("x".repeat(513)), json!("\n")] {
        let mut body = websocket_body(json!(""));
        body["generate"] = json!(false);
        body["client_metadata"]["x-codex-turn-metadata"] =
            json!(json!({"turn_id":nested}).to_string());
        assert!(parse_websocket(&HeaderMap::new(), body, Uuid::new_v4()).is_err());
    }
    let mut body = websocket_body(json!(""));
    body["generate"] = json!(false);
    body["client_metadata"]["window_id"] = json!("");
    assert!(parse_websocket(&HeaderMap::new(), body, Uuid::new_v4()).is_err());

    let body = websocket_body(json!("x".repeat(512)));
    assert!(parse_websocket(&HeaderMap::new(), body, Uuid::new_v4()).is_ok());
}

#[test]
fn websocket_turns_use_each_message_and_never_reuse_handshake_turn_metadata() {
    let headers = HeaderMap::from_iter([
        ("session-id".parse().unwrap(), "session".parse().unwrap()),
        ("thread-id".parse().unwrap(), "thread".parse().unwrap()),
        ("x-codex-turn-metadata".parse().unwrap(),
            r#"{"turn_id":"handshake-turn","parent_turn_id":"handshake-parent","request_kind":"prewarm"}"#.parse().unwrap()),
    ]);
    let session = Uuid::new_v4();
    for turn in ["turn-1", "turn-2"] {
        let body = websocket_body(json!(turn));
        let parsed = parse_websocket(&headers, body.clone(), session).unwrap();
        assert_eq!(parsed.identity.turn_id.as_deref(), Some(turn));
        assert_eq!(parsed.identity.session_id, "session");
        assert_eq!(parsed.identity.thread_id, "thread");
        assert!(
            !parsed
                .identity_headers
                .contains_key("x-codex-turn-metadata")
        );

        // The HTTP endpoint still checks agreement between headers and body.
        let mut http_body = body;
        http_body.as_object_mut().unwrap().remove("type");
        assert_eq!(
            ResponsesIngress
                .parse(&headers, http_body)
                .err()
                .unwrap()
                .code,
            "identity_conflict"
        );
    }
    let parsed = parse_websocket(
        &headers,
        json!({"type":"response.create","model":"test-model","input":[]}),
        session,
    )
    .unwrap();
    assert!(parsed.identity.turn_id.is_none());
    assert_eq!(
        headers["x-codex-turn-metadata"],
        r#"{"turn_id":"handshake-turn","parent_turn_id":"handshake-parent","request_kind":"prewarm"}"#
    );

    // Conflicting sources within the same message remain invalid.
    let mut body = websocket_body(json!("turn-1"));
    body["client_metadata"]["x-codex-turn-metadata"] = json!(r#"{"turn_id":"turn-2"}"#);
    assert_eq!(
        parse_websocket(&headers, body, session).err().unwrap().code,
        "identity_conflict"
    );
}
