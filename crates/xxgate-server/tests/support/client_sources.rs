use super::*;

async fn policy(c: &Client, id: &str, codex_only: bool) {
    let a = c.admin(&format!("/accounts/{id}"), "GET", None).await["account"].clone();
    let updated=c.admin(&format!("/accounts/{id}"),"PUT",Some(json!({"version":a["version"],"name":a["name"],"models":a["models"],"models_restricted":a["models_restricted"],"max_inflight":a["max_inflight"],"codex_only":codex_only}))).await;
    assert_eq!(updated["codex_only"], codex_only);
}

async fn call(c: &Client, ua: &str, body: Value) -> (u16, String, Value) {
    let response = c
        .http
        .post(format!("{}/v1/responses", c.base))
        .bearer_auth(&c.key)
        .header("user-agent", ua)
        .json(&body)
        .send()
        .await
        .unwrap();
    let status = response.status().as_u16();
    let id = response.headers()["x-request-id"]
        .to_str()
        .unwrap()
        .to_owned();
    let body = response.json().await.unwrap();
    (status, id, body)
}

pub(super) async fn verify(c: &Client, store: &PgStore, mock: &Mock, a: &str, b: &str) {
    c.enabled(a, true).await;
    c.enabled(b, true).await;
    policy(c, a, true).await;
    policy(c, b, false).await;
    let body = json!({"model":"mock-model","input":"PRIVATE_SOURCE_TEST"});
    let (status, id, _) = call(c, "Go-http-client/1.1", body.clone()).await;
    assert_eq!(status, 200);
    let request = c.record(&id).await["request"].clone();
    assert_eq!(request["account_id"], b);
    assert_eq!(request["client_origin"]["source"], "unknown");
    assert_eq!(request["stateless"], true);
    assert_eq!(
        request["ingress_diagnostics"]["client_origin"]["source"],
        "unknown"
    );
    {
        let captures = mock.captures.lock().await;
        let (headers, body) = captures.last().unwrap();
        assert!(
            headers["user-agent"]
                .to_str()
                .unwrap()
                .starts_with("codex_cli_rs/")
        );
        for name in [
            "session-id",
            "session_id",
            "conversation_id",
            "thread-id",
            "x-client-request-id",
            "x-codex-turn-metadata",
            "x-codex-installation-id",
            "x-codex-window-id",
        ] {
            assert!(!headers.contains_key(name));
        }
        assert!(body.get("service_tier").is_none());
        assert!(body.get("client_metadata").is_none());
        assert!(body.get("prompt_cache_key").is_none());
    }
    // A cache partition is stable but never evidence of Codex or a real session.
    // OAuth affinity headers may reuse it without creating an internal binding.
    let mut cache_keys = vec![];
    for _ in 0..2 {
        let (status,id,_)=call(c,"opencode/1.0",json!({"model":"mock-model","input":"PRIVATE_SOURCE_TEST","prompt_cache_key":"customer-cache"})).await;
        assert_eq!(status, 200);
        let detail = c.record(&id).await;
        let r = &detail["request"];
        assert_eq!(r["client_origin"]["source"], "unknown");
        assert_eq!(r["stateless"], true);
        assert_eq!(r["client_session_id"], "");
        assert!(r["binding_id"].is_null());
        assert!(detail["bindings"].as_array().unwrap().is_empty());
        let captures = mock.captures.lock().await;
        let (headers, body) = captures.last().unwrap();
        let cache_key = body["prompt_cache_key"].as_str().unwrap();
        for field in ["session_id", "conversation_id"] {
            assert_eq!(headers[field].to_str().unwrap(), cache_key);
            let entries = detail["events"]
                .as_array()
                .unwrap()
                .iter()
                .find(|e| e["kind"] == "request_rewritten")
                .unwrap()["details"]["entries"]
                .as_array()
                .unwrap();
            let entry = entries
                .iter()
                .find(|e| e["field"] == format!("headers.{field}"))
                .unwrap();
            assert!(entry["before"].is_null());
            assert_eq!(entry["after"], cache_key);
            assert_eq!(entry["action"], "added");
        }
        for field in [
            "session-id",
            "thread-id",
            "x-client-request-id",
            "x-codex-turn-metadata",
        ] {
            assert!(!headers.contains_key(field));
        }
        assert!(body.get("client_metadata").is_none());
        assert!(body.get("include").is_none()); // No unrelated body adaptation.
        cache_keys.push(body["prompt_cache_key"].clone());
    }
    assert_eq!(cache_keys[0], cache_keys[1]);
    assert_ne!(cache_keys[0], "customer-cache");
    let secret = c
        .admin(
            "/keys",
            "POST",
            Some(json!({"name":"Cache namespace test"})),
        )
        .await["secret"]
        .as_str()
        .unwrap()
        .to_owned();
    let other = Client {
        http: c.http.clone(),
        base: c.base.clone(),
        key: secret,
    };
    let (status,_,_)=call(&other,"Go-http-client/1.1",json!({"model":"mock-model","input":"PRIVATE_SOURCE_TEST","prompt_cache_key":"customer-cache"})).await;
    assert_eq!(status, 200);
    assert_ne!(
        mock.captures.lock().await.last().unwrap().1["prompt_cache_key"],
        cache_keys[0]
    );
    policy(c, b, true).await;
    for (ua, visible) in [
        ("Go-http-client/1.1", false),
        ("codex_cli_rs/0.153.4", true),
    ] {
        let models: Value = c
            .http
            .get(format!("{}/v1/models", c.base))
            .bearer_auth(&c.key)
            .header("user-agent", ua)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(
            models["data"]
                .as_array()
                .unwrap()
                .iter()
                .any(|m| m["id"] == "mock-model"),
            visible
        );
    }
    let calls = mock.calls.load(Ordering::SeqCst);
    for generic in [
        body.clone(),
        json!({"model":"mock-model","input":"PRIVATE","prompt_cache_key":"not-a-codex-session"}),
        json!({"model":"mock-model","input":"PRIVATE","client_metadata":{"session_id":"has-a-session","thread_id":"has-a-thread"}}),
    ] {
        let (status, id, error) = call(c, "Go-http-client/1.1", generic).await;
        assert_eq!(status, 403);
        assert_eq!(error["error"]["code"], "client_source_not_allowed");
        let r = c.record(&id).await["request"].clone();
        assert_eq!(r["upstream_attempts"], 0);
        assert_eq!(r["client_origin"]["source"], "unknown");
    }
    assert_eq!(mock.calls.load(Ordering::SeqCst), calls);
    // Recognized Codex does not have to supply a session either.
    let (status, id, _) = call(c, "codex_cli_rs/0.153.4", body.clone()).await;
    assert_eq!(status, 200);
    let r = c.record(&id).await["request"].clone();
    assert_eq!(r["stateless"], true);
    assert_eq!(r["client_origin"]["source"], "codex");
    // Proxies may replace the UA: complete Codex metadata is an independent rule.
    let (status,id,_)=call(c,"Go-http-client/1.1",json!({"model":"mock-model","input":"PRIVATE","client_metadata":{"session_id":"source-session","thread_id":"source-thread","turn_id":"source-turn","x-codex-installation-id":"source-installation","x-codex-window-id":"source-window"}})).await;
    assert_eq!(status, 200);
    let r = c.record(&id).await["request"].clone();
    assert_eq!(r["stateless"], false);
    assert_eq!(r["client_origin"]["source"], "codex");
    assert_eq!(r["client_origin"]["rule"], "codex_client_metadata");
    assert_eq!(r["client_session_id"], "source-session");
    {
        let captures = mock.captures.lock().await;
        let (headers, body) = captures.last().unwrap();
        assert_eq!(
            headers["session-id"].to_str().unwrap(),
            body["client_metadata"]["session_id"].as_str().unwrap()
        );
        assert_eq!(
            headers["thread-id"].to_str().unwrap(),
            body["client_metadata"]["thread_id"].as_str().unwrap()
        );
        assert_eq!(headers["x-client-request-id"], headers["thread-id"]);
        assert!(body.get("prompt_cache_key").is_none());
        assert_ne!(body["client_metadata"]["session_id"], "source-session");
        assert!(body["client_metadata"].get("context_window_id").is_none());
        assert!(
            body["client_metadata"]
                .get("x-codex-turn-metadata")
                .is_none()
        );
    }
    // sub2api's API-key path can preserve body metadata while omitting all
    // identity headers. Repeated calls must project the same mapped identities.
    let turn = json!({"session_id":"body-session","thread_id":"body-session","turn_id":"body-turn","root_turn_id":"body-turn","installation_id":"body-install","window_id":"body-window","context_window_id":"body-context","parent_thread_id":"body-parent","tool_namespaces_info":"PRIVATE_TOOL_METADATA","unrelated":"PRIVATE_METADATA"});
    let body = json!({"model":"mock-model","input":[{"role":"user","content":"PRIVATE_PROJECTION_TEST"}],"prompt_cache_key":"body-session","client_metadata":{"session_id":"body-session","thread_id":"body-session","x-codex-installation-id":"body-install","x-codex-window-id":"body-window","x-codex-turn-metadata":turn.to_string()}});
    let mut first_headers = None;
    for _ in 0..2 {
        let (status, id, _) = call(c, "Go-http-client/1.1", body.clone()).await;
        assert_eq!(status, 200);
        let detail = c.record(&id).await;
        let captures = mock.captures.lock().await;
        let (headers, forwarded) = captures.last().unwrap();
        let nested: Value = serde_json::from_str(
            forwarded["client_metadata"]["x-codex-turn-metadata"]
                .as_str()
                .unwrap(),
        )
        .unwrap();
        let header_turn: Value =
            serde_json::from_str(headers["x-codex-turn-metadata"].to_str().unwrap()).unwrap();
        for field in [
            "session_id",
            "thread_id",
            "turn_id",
            "root_turn_id",
            "installation_id",
            "window_id",
            "context_window_id",
            "parent_thread_id",
        ] {
            assert_eq!(header_turn[field], nested[field]);
        }
        for (header, field) in [
            ("session-id", "session_id"),
            ("thread-id", "thread_id"),
            ("x-codex-installation-id", "installation_id"),
            ("x-codex-window-id", "window_id"),
            ("x-codex-parent-thread-id", "parent_thread_id"),
        ] {
            let value = headers[header].to_str().unwrap();
            assert_eq!(value, nested[field].as_str().unwrap());
            let event = detail["events"]
                .as_array()
                .unwrap()
                .iter()
                .find(|e| e["kind"] == "request_rewritten")
                .unwrap();
            let entry = event["details"]["entries"]
                .as_array()
                .unwrap()
                .iter()
                .find(|e| e["field"] == format!("headers.{header}"))
                .unwrap();
            assert!(entry["before"].is_null());
            assert_eq!(entry["after"], value);
            assert_eq!(entry["action"], "added");
        }
        assert_eq!(headers["x-client-request-id"], headers["thread-id"]);
        assert_eq!(
            headers["session-id"].to_str().unwrap(),
            forwarded["prompt_cache_key"].as_str().unwrap()
        );
        assert_eq!(forwarded["input"], body["input"]);
        assert!(
            !headers["x-codex-turn-metadata"]
                .to_str()
                .unwrap()
                .contains("PRIVATE_")
        );
        if let Some(first) = &first_headers {
            assert_eq!(first, headers);
        } else {
            first_headers = Some(headers.clone());
        }
    }
    // Model-only edits must not silently turn off an existing source policy.
    let current = c.admin(&format!("/accounts/{a}"), "GET", None).await["account"].clone();
    let updated=c.admin(&format!("/accounts/{a}"),"PUT",Some(json!({"version":current["version"],"name":current["name"],"models":current["models"],"models_restricted":current["models_restricted"],"max_inflight":current["max_inflight"]}))).await;
    assert_eq!(updated["codex_only"], true);
    assert!(
        store
            .accounts()
            .await
            .unwrap()
            .iter()
            .find(|x| x.id.to_string() == a)
            .unwrap()
            .codex_only
    );
    let filtered = c.admin("/requests?client_source=codex", "GET", None).await;
    assert!(filtered["total"].as_i64().unwrap() >= 2);
    assert!(
        filtered["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|r| r["client_origin"]["source"] == "codex")
    );
    policy(c, a, false).await;
    policy(c, b, false).await;
    c.enabled(b, false).await;
    verify_output_limit_compatibility(c, mock).await;
}

async fn verify_output_limit_compatibility(c: &Client, mock: &Mock) {
    for path in ["/v1/responses", "/responses"] {
        for stream in [false, true] {
            let input =
                json!([{"role":"user","content":"PRIVATE_TEXT mentioning max_output_tokens"}]);
            let tools = json!([{"type":"function","name":"test_limit","parameters":{"type":"object","properties":{"max_output_tokens":{"type":"integer"}}}}]);
            let response = c
                .http
                .post(format!("{}{path}", c.base))
                .bearer_auth(&c.key)
                .json(
                    &json!({"model":"mock-model","input":input,"tools":tools,"stream":stream,
                    "max_output_tokens":if stream {json!(32)} else {Value::Null}}),
                )
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), 200);
            let id = response.headers()["x-request-id"]
                .to_str()
                .unwrap()
                .to_owned();
            let text = response.text().await.unwrap();
            if stream {
                assert!(text.contains("response.completed"));
                assert!(!text.contains("response.failed"));
            } else {
                assert_eq!(
                    serde_json::from_str::<Value>(&text).unwrap()["status"],
                    "completed"
                );
            }
            let record = c.record(&id).await;
            assert_eq!(record["request"]["state"], "completed");
            assert_eq!(record["request"]["upstream_attempts"], 1);
            // Keep actual usage even when it exceeds the ignored requested cap.
            assert_eq!(record["request"]["usage"]["output_tokens"], 500);
            let captures = mock.captures.lock().await;
            let forwarded = &captures.last().unwrap().1;
            assert!(forwarded.get("max_output_tokens").is_none());
            assert_eq!(forwarded["input"], input);
            assert_eq!(forwarded["tools"], tools);
        }
    }
}
