use super::*;

pub(super) async fn verify(c: &Client, store: &PgStore, mock: &Mock) -> String {
    let body = json!({"model":"mock-model","input":"PRIVATE_COMPAT_PROMPT","instructions":"normal","stream":false});
    let response = c
        .http
        .post(format!("{}/v1/responses", c.base))
        .bearer_auth(&c.key)
        .header("session_id", "compat-session")
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let first_id = response.headers()["x-request-id"]
        .to_str()
        .unwrap()
        .to_owned();
    response.bytes().await.unwrap();
    let first = c.record(&first_id).await;
    assert_eq!(first["request"]["client_session_id"], "compat-session");
    assert_eq!(first["request"]["client_thread_id"], "compat-session");
    assert_eq!(
        first["request"]["ingress_diagnostics"]["identity"]["resolved"]["session_sources"],
        json!(["headers.session_id[0]"])
    );
    assert_eq!(
        first["request"]["ingress_diagnostics"]["identity"]["resolved"]["thread_sources"],
        json!(["resolved.session_id"])
    );
    assert!(!first.to_string().contains("PRIVATE_"));
    let response = c
        .http
        .post(format!("{}/v1/responses", c.base))
        .bearer_auth(&c.key)
        .header("session-id", "compat-session")
        .header("thread-id", "compat-session")
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let id = response.headers()["x-request-id"]
        .to_str()
        .unwrap()
        .to_owned();
    response.bytes().await.unwrap();
    assert_eq!(
        c.record(&id).await["request"]["binding_id"],
        first["request"]["binding_id"]
    );

    let conflict = c
        .http
        .post(format!("{}/v1/responses", c.base))
        .bearer_auth(&c.key)
        .header("session-id", "one")
        .header("session_id", "two")
        .header("x-client-request-id", "client-correlation-id")
        .header("cookie", "PRIVATE_COOKIE")
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(conflict.status(), 400);
    let conflict_id = conflict.headers()["x-request-id"]
        .to_str()
        .unwrap()
        .to_owned();
    assert_eq!(
        conflict.json::<Value>().await.unwrap()["error"]["code"],
        "identity_conflict"
    );
    let record = c.record(&conflict_id).await;
    assert_eq!(record["request"]["upstream_attempts"], 0);
    let d = &record["request"]["ingress_diagnostics"];
    assert_eq!(d["failure_stage"], "ingress_validation");
    assert_eq!(
        d["headers"]["x-client-request-id"]["value"],
        "client-correlation-id"
    );
    assert_eq!(d["identity"]["fields"][0]["value"], "one");
    assert_eq!(d["identity"]["fields"][1]["value"], "two");
    assert_eq!(d["body_fields"]["input"]["type"], "string");
    assert!(!record.to_string().contains("PRIVATE_"));
    assert!(
        record["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["kind"] == "request_rejected"
                && e["details"]["diagnostics"]["failure_stage"] == "ingress_validation")
    );

    let bindings_before = store.active_bindings().await.unwrap().len();
    for stream in [false, true] {
        let response = c
            .http
            .post(format!("{}/v1/responses", c.base))
            .bearer_auth(&c.key)
            .json(&json!({"model":"mock-model","input":"PRIVATE_STATELESS_INPUT","stream":stream}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let id = response.headers()["x-request-id"]
            .to_str()
            .unwrap()
            .to_owned();
        response.bytes().await.unwrap();
        let record = c.record(&id).await;
        assert_eq!(record["request"]["state"], "completed");
        assert_eq!(record["request"]["stateless"], true);
        assert_eq!(record["request"]["client_session_id"], "");
        assert_eq!(record["request"]["client_thread_id"], "");
        assert!(record["request"]["binding_id"].is_null());
        assert!(record["request"]["cache_previous"].is_null());
        assert_eq!(record["request"]["usage"]["cached_input_tokens"], 400);
        assert_eq!(record["request"]["upstream_attempts"], 1);
        assert_eq!(
            record["request"]["ingress_diagnostics"]["identity"]["status"],
            "stateless"
        );
        assert_eq!(record["bindings"], json!([]));
        assert_eq!(record["mappings"], json!([]));
        assert!(!record.to_string().contains("PRIVATE_"));
        let captures = mock.captures.lock().await;
        let (headers, upstream) = captures.last().unwrap();
        assert!(upstream.get("prompt_cache_key").is_none());
        assert_eq!(upstream["input"][0]["content"], "PRIVATE_STATELESS_INPUT");
        assert_eq!(upstream["instructions"], "");
        for name in [
            "session-id",
            "thread-id",
            "x-client-request-id",
            "x-codex-window-id",
            "x-codex-turn-metadata",
        ] {
            assert!(
                !headers.contains_key(name),
                "Unexpected generated header: {name}"
            );
        }
        assert!(upstream.get("client_metadata").is_none());
    }
    // Ordinary two-message input retains its text while adopting the upstream role.
    let response = c.http.post(format!("{}/v1/responses",c.base)).bearer_auth(&c.key)
        .json(&json!({"model":"mock-model","input":[{"role":"system","content":"PRIVATE_SYSTEM"},{"role":"user","content":"PRIVATE_USER"}]}))
        .send().await.unwrap();
    assert_eq!(response.status(), 200);
    response.bytes().await.unwrap();
    {
        let captures = mock.captures.lock().await;
        let upstream = &captures.last().unwrap().1;
        assert_eq!(
            upstream["input"][0],
            json!({"role":"developer","content":"PRIVATE_SYSTEM"})
        );
        assert_eq!(
            upstream["input"][1],
            json!({"role":"user","content":"PRIVATE_USER"})
        );
        assert!(upstream.get("prompt_cache_key").is_none());
    }
    assert_eq!(
        store.active_bindings().await.unwrap().len(),
        bindings_before
    );

    let invalid = c
        .http
        .post(format!("{}/v1/responses", c.base))
        .bearer_auth(&c.key)
        .body("PRIVATE_INVALID_JSON")
        .send()
        .await
        .unwrap();
    assert_eq!(invalid.status(), 400);
    let id = invalid.headers()["x-request-id"]
        .to_str()
        .unwrap()
        .to_owned();
    invalid.bytes().await.unwrap();
    let record = c.record(&id).await;
    assert_eq!(
        record["request"]["ingress_diagnostics"]["failure_stage"],
        "json_parse"
    );
    assert_eq!(
        record["request"]["ingress_diagnostics"]["body_status"],
        "invalid_json"
    );
    assert!(!record.to_string().contains("PRIVATE_"));

    let unauthorized = c
        .http
        .post(format!("{}/v1/responses", c.base))
        .bearer_auth("PRIVATE_INVALID_KEY")
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(unauthorized.status(), 401);
    let id = unauthorized.headers()["x-request-id"]
        .to_str()
        .unwrap()
        .to_owned();
    unauthorized.bytes().await.unwrap();
    let record = c.record(&id).await;
    assert_eq!(
        record["request"]["ingress_diagnostics"]["failure_stage"],
        "authentication"
    );
    assert_eq!(
        record["request"]["ingress_diagnostics"]["body_status"],
        "not_parsed"
    );
    assert!(!record.to_string().contains("PRIVATE_"));
    conflict_id
}
