use super::*;

fn compact_output() -> Value {
    json!({"id":"cmp_response_original","object":"response.compaction","created_at":123,"model":"mock-compact-returned",
        "output":[
            {"type":"message","id":"msg_retained","role":"user","content":[{"type":"input_text","text":"PRIVATE_RETAINED_TEXT"}]},
            {"type":"image_generation_call","id":"ig_historical","result":"PRIVATE_HISTORICAL_IMAGE","usage":{"input_tokens":999,"output_tokens":999}},
            {"type":"compaction","id":"cmp_original","encrypted_content":"PRIVATE_COMPACT_CIPHERTEXT","future":{"keep":true}}
        ],"usage":{"input_tokens":1000,"input_tokens_details":{"cached_tokens":400},
            "output_tokens":500,"output_tokens_details":{"reasoning_tokens":200},
            "total_tokens":1500,"future_tokens":42,"private_text":"PRIVATE_USAGE_TEXT"},
        "service_tier":"default","future":{"keep":true}})
}

pub(super) async fn upstream_compact(
    State(mock): State<Mock>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    assert_eq!(headers["accept"], "application/json");
    assert_eq!(headers["authorization"], "Bearer PRIVATE_ACCESS_TOKEN_A");
    assert_eq!(headers["chatgpt-account-id"], "mock-account-A");
    assert_eq!(body["model"], "mock-upstream");
    assert!(body.get("stream").is_none());
    assert!(body.get("store").is_none());
    mock.compact.lock().await.push((headers, body.clone()));
    let mut value = compact_output();
    let bytes = match body["instructions"].as_str().unwrap_or("") {
        "http400" => return (StatusCode::BAD_REQUEST, Json(json!({"error":{"code":"invalid_encrypted_content","message":"PRIVATE_COMPACT_ERROR"}}))).into_response(),
        "invalid" => b"{broken".to_vec(),
        "wrong-shape" => b"{\"object\":\"response\",\"output\":[]}".to_vec(),
        "no-usage" => { value.as_object_mut().unwrap().remove("usage"); serde_json::to_vec(&value).unwrap() },
        "large" => { value["extra"] = json!("x".repeat(2048)); serde_json::to_vec(&value).unwrap() },
        "cut" => serde_json::to_vec(&value).unwrap()[..20].to_vec(),
        _ => serde_json::to_vec(&value).unwrap(),
    };
    let mut response = if body["instructions"] == "wait" {
        let (tx, rx) = mpsc::channel::<Result<Bytes, Infallible>>(1);
        tokio::spawn(async move {
            let _ = tx.send(Ok(Bytes::from_static(b" "))).await;
            tx.closed().await;
        });
        Body::from_stream(ReceiverStream::new(rx)).into_response()
    } else {
        Body::from(bytes).into_response()
    };
    response
        .headers_mut()
        .insert("content-type", "application/json".parse().unwrap());
    response
        .headers_mut()
        .insert("x-request-id", "official-compact-request".parse().unwrap());
    response
        .headers_mut()
        .insert("x-codex-turn-state", "opaque-turn-state".parse().unwrap());
    response
}

async fn send(c: &Client, path: &str, body: Value) -> reqwest::Response {
    c.http
        .post(format!("{}{path}", c.base))
        .bearer_auth(&c.key)
        .header("x-codex-turn-state", "client-compact-state")
        // Operation metadata in the body must supplement this partial header.
        .header(
            "x-codex-turn-metadata",
            json!({"session_id":"session-compact","thread_id":"session-compact"}).to_string(),
        )
        .json(&body)
        .send()
        .await
        .unwrap()
}

pub(super) async fn verify(c: &Client, gateway: &Gateway, mock: &Mock, a: &str, b: &str) {
    c.enabled(a, true).await;
    c.enabled(b, false).await;
    let meta = json!({"session_id":"session-compact","thread_id":"session-compact","request_kind":"compaction",
        "compaction":{"trigger":"auto","reason":"context_limit","implementation":"responses_compact","phase":"mid_turn","strategy":"memento"}});
    let body = json!({"model":"mock-model","instructions":"normal","input":[
        {"type":"compaction","id":"cmp_old","encrypted_content":"PRIVATE_COMPACT_HISTORY"},
        {"type":"message","id":"msg_original","role":"user","content":"PRIVATE_INPUT"}],
        "client_metadata":{"x-codex-turn-metadata":meta.to_string()},"prompt_cache_key":"session-compact",
        "tools":[],"parallel_tool_calls":true,"reasoning":{"effort":"high"},"text":{"verbosity":"low"},"access_programs":[]});

    // A queued unary Compact must not emit an SSE response or heartbeats.
    let hold = c.response("session-compact", "hold", true, "default").await;
    let pending = send(c, "/v1/responses/compact", body.clone());
    tokio::pin!(pending);
    tokio::select! {
        _=&mut pending=>panic!("Compact returned while still queued"),
        ()=wait_queue(gateway,1)=>{}
    }
    tokio::select! {
        _=&mut pending=>panic!("Compact emitted a queue heartbeat"),
        ()=tokio::time::sleep(Duration::from_millis(220))=>{}
    }
    mock.release.add_permits(1);
    hold.text().await.unwrap();
    let response = pending.await;
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["content-type"], "application/json");
    assert_eq!(
        response.headers()["x-codex-turn-state"],
        "opaque-turn-state"
    );
    let id = response.headers()["x-request-id"]
        .to_str()
        .unwrap()
        .to_owned();
    assert_eq!(
        response.bytes().await.unwrap().as_ref(),
        serde_json::to_vec(&compact_output()).unwrap()
    );
    let capture = mock.compact.lock().await.last().unwrap().clone();
    assert!(!capture.0.contains_key("x-codex-turn-state"));
    assert_eq!(capture.1["input"], body["input"]);
    for field in [
        "tools",
        "parallel_tool_calls",
        "reasoning",
        "text",
        "access_programs",
    ] {
        assert_eq!(capture.1[field], body[field]);
    }
    assert_eq!(
        capture.0["session-id"].to_str().unwrap(),
        capture.1["prompt_cache_key"].as_str().unwrap()
    );
    let projected: Value =
        serde_json::from_slice(capture.0["x-codex-turn-metadata"].as_bytes()).unwrap();
    assert_eq!(projected["compaction"], meta["compaction"]);
    assert_eq!(projected["request_kind"], "compaction");
    let record = c.record(&id).await;
    assert_eq!(record["request"]["kind"], "compact");
    assert_eq!(record["request"]["upstream_model"], "mock-upstream");
    assert_eq!(record["request"]["response_model"], "mock-compact-returned");
    assert_eq!(
        record["request"]["compaction"],
        json!({"method":"compact","output_observed":true})
    );
    assert_eq!(record["request"]["usage"]["input_tokens"], 1000);
    assert_eq!(record["request"]["usage"]["cached_input_tokens"], 400);
    assert_eq!(record["request"]["usage"]["output_tokens"], 500);
    assert_eq!(record["request"]["usage"]["reasoning_output_tokens"], 200);
    assert_eq!(record["request"]["usage"]["raw_usage"]["future_tokens"], 42);
    assert_eq!(record["request"]["usage"]["image_count"], 0);
    assert_eq!(record["request"]["usage"]["source"], "upstream_compaction");
    assert_eq!(record["request"]["valuation"]["status"], "priced");
    assert_eq!(
        record["request"]["valuation"]["cny"]
            .as_str()
            .unwrap()
            .parse::<rust_decimal::Decimal>()
            .unwrap(),
        rust_decimal::Decimal::new(164, 4)
    );
    assert_eq!(
        record["request"]["upstream_request_id"],
        "official-compact-request"
    );
    assert!(!record.to_string().contains("PRIVATE_"));
    assert_eq!(
        record["request"]["client_turn_state"]["values"][0]["value"],
        "client-compact-state"
    );
    let attempt = record["events"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["kind"] == "upstream_attempt_headers")
        .unwrap();
    assert_eq!(
        attempt["details"]["turn_state"]["values"][0]["value"],
        "opaque-turn-state"
    );
    let list = c.admin("/requests?kind=compact", "GET", None).await;
    assert_eq!(list["total"], 1);
    assert!(list["items"][0].get("compaction").is_none());

    // The full compact output can be supplied as the next context without rewriting ciphertext or IDs.
    let mut continuation = body.clone();
    continuation
        .as_object_mut()
        .unwrap()
        .remove("access_programs");
    continuation["input"] = compact_output()["output"].clone();
    continuation["client_metadata"] = json!({});
    let normal = send(c, "/responses", continuation.clone()).await;
    let normal_id = normal.headers()["x-request-id"]
        .to_str()
        .unwrap()
        .to_owned();
    assert_eq!(normal.status(), 200);
    normal.bytes().await.unwrap();
    assert!(
        c.record(&normal_id).await["request"]
            .get("compaction")
            .is_none()
    );
    assert_eq!(
        mock.captures.lock().await.last().unwrap().1["input"],
        continuation["input"]
    );
    assert_eq!(
        c.record(&normal_id).await["request"]["binding_id"],
        record["request"]["binding_id"]
    );

    let mut v2 = continuation;
    v2["max_output_tokens"] = json!(128);
    v2["instructions"] = json!("compact-v2");
    v2["input"]
        .as_array_mut()
        .unwrap()
        .push(json!({"type":"compaction_trigger"}));
    let mut v2meta = meta.clone();
    v2meta["compaction"]["implementation"] = json!("responses_compaction_v2");
    v2["client_metadata"] = json!({"x-codex-turn-metadata":v2meta.to_string()});
    for streaming in [true, false] {
        v2["stream"] = json!(streaming);
        let response = send(c, "/v1/responses", v2.clone()).await;
        assert_eq!(response.status(), 200);
        let id = response.headers()["x-request-id"]
            .to_str()
            .unwrap()
            .to_owned();
        let text = response.text().await.unwrap();
        assert!(text.contains("PRIVATE_V2_CIPHERTEXT"));
        assert!(text.contains("cmp_v2"));
        if streaming {
            assert!(text.contains("response.output_item.done"));
            assert!(text.contains("response.completed"));
        }
        let r = c.record(&id).await;
        assert_eq!(r["request"]["kind"], "responses");
        assert_eq!(
            r["request"]["compaction"],
            json!({"method":"remote_v2","output_observed":true})
        );
        assert_eq!(r["request"]["usage"]["input_tokens"], 1000);
        assert!(!r.to_string().contains("PRIVATE_"));
        let captured = mock.captures.lock().await.last().unwrap().clone();
        assert_eq!(captured.1["input"], v2["input"]);
        let projected: Value =
            serde_json::from_slice(captured.0["x-codex-turn-metadata"].as_bytes()).unwrap();
        assert_eq!(projected["compaction"], v2meta["compaction"]);
    }
    for (behavior, state, observed) in [
        ("normal", "completed", json!(false)),
        ("streamfail", "failed", Value::Null),
    ] {
        v2["instructions"] = json!(behavior);
        let response = send(c, "/responses", v2.clone()).await;
        let id = response.headers()["x-request-id"]
            .to_str()
            .unwrap()
            .to_owned();
        response.bytes().await.unwrap();
        let r = c.record(&id).await;
        assert_eq!(r["request"]["state"], state);
        assert_eq!(r["request"]["compaction"]["output_observed"], observed);
    }

    for (behavior, status) in [
        ("http400", 400),
        ("invalid", 502),
        ("wrong-shape", 502),
        ("cut", 502),
        ("no-usage", 200),
    ] {
        let mut body = body.clone();
        body["instructions"] = json!(behavior);
        let before = mock.compact.lock().await.len();
        let response = send(c, "/responses/compact", body).await;
        assert_eq!(response.status(), status);
        let id = response.headers()["x-request-id"]
            .to_str()
            .unwrap()
            .to_owned();
        response.bytes().await.unwrap();
        let r = c.record(&id).await;
        assert_eq!(r["request"]["upstream_attempts"], 1);
        assert_eq!(mock.compact.lock().await.len(), before + 1);
        assert_eq!(r["request"]["usage"]["complete"], false);
        assert!(r["request"]["valuation"]["cny"].is_null());
        assert!(!r.to_string().contains("PRIVATE_"));
    }
    let before = mock.compact.lock().await.len();
    let mut streaming = body.clone();
    streaming["stream"] = json!(true);
    let rejected = send(c, "/responses/compact", streaming).await;
    assert_eq!(rejected.status(), 400);
    let id = rejected.headers()["x-request-id"]
        .to_str()
        .unwrap()
        .to_owned();
    rejected.bytes().await.unwrap();
    let rejected = c.record(&id).await;
    assert_eq!(rejected["request"]["upstream_attempts"], 0);
    assert_eq!(rejected["request"]["compaction"]["method"], "compact");
    assert_eq!(mock.compact.lock().await.len(), before);

    let settings = c.admin("/settings", "GET", None).await;
    let mut limit = settings.clone();
    limit["sse_idle_timeout_ms"] = json!(100);
    limit["sse_event_limit_bytes"] = json!(1024);
    c.admin("/settings", "PUT", Some(limit)).await;
    for (behavior, code) in [
        ("large", "compact_response_too_large"),
        ("wait", "compact_timeout"),
    ] {
        let mut body = body.clone();
        body["instructions"] = json!(behavior);
        let response = send(c, "/responses/compact", body).await;
        assert!(response.status().is_server_error());
        assert_eq!(
            response.json::<Value>().await.unwrap()["error"]["code"],
            code
        );
        wait_inflight(gateway, 0).await;
    }
    let mut reset = c.admin("/settings", "GET", None).await;
    reset["sse_idle_timeout_ms"] = settings["sse_idle_timeout_ms"].clone();
    reset["sse_event_limit_bytes"] = settings["sse_event_limit_bytes"].clone();
    c.admin("/settings", "PUT", Some(reset)).await;

    let count = mock.compact.lock().await.len();
    let mut waiting = body;
    waiting["instructions"] = json!("wait");
    use tokio::io::AsyncWriteExt;
    let mut socket = tokio::net::TcpStream::connect(c.base.trim_start_matches("http://"))
        .await
        .unwrap();
    let payload = waiting.to_string();
    let wire = format!(
        "POST /responses/compact HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
        c.key,
        payload.len(),
        payload
    );
    socket.write_all(wire.as_bytes()).await.unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        while mock.compact.lock().await.len() == count {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    drop(socket);
    wait_inflight(gateway, 0).await;
}
