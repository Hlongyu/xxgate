use super::*;

pub(super) fn rejection(mock: &Mock, body: &Value, behavior: &str) -> Option<Response> {
    if !behavior.starts_with("encrypted-") || behavior == "encrypted-sse" {
        return None;
    }
    let encrypted = body["input"]
        .as_array()
        .unwrap()
        .iter()
        .any(|item| item["type"] == "reasoning" && item.get("encrypted_content").is_some());
    if !encrypted && behavior != "encrypted-always" {
        return None;
    }
    let code = if behavior == "encrypted-wrong-code" {
        "invalid_request_error"
    } else {
        "invalid_encrypted_content"
    };
    let status = if behavior == "encrypted-server-error" {
        StatusCode::INTERNAL_SERVER_ERROR
    } else {
        StatusCode::BAD_REQUEST
    };
    let error = json!({"error":{"code":code,"message":"PRIVATE_UPSTREAM_ERROR_TEXT invalid_encrypted_content"}}).to_string();
    let mut response = if behavior.starts_with("encrypted-held-error") {
        let mock = mock.clone();
        let (tx, rx) = mpsc::channel(2);
        tokio::spawn(async move {
            let _ = tx.send(Ok::<_, Infallible>(Bytes::from_static(b" "))).await;
            let permit = mock.release.acquire().await.unwrap();
            permit.forget();
            let _ = tx.send(Ok(Bytes::from(error))).await;
        });
        (status, Body::from_stream(ReceiverStream::new(rx))).into_response()
    } else {
        (status, error).into_response()
    };
    response
        .headers_mut()
        .insert("x-request-id", "mock-rejected-attempt".parse().unwrap());
    response
        .headers_mut()
        .insert("x-codex-turn-state", "rejected-turn-state".parse().unwrap());
    Some(response)
}

async fn request(c: &Client, behavior: &str, stream: bool) -> reqwest::Response {
    c.http.post(format!("{}/v1/responses", c.base))
        .bearer_auth(&c.key)
        .header("x-codex-turn-state", "client-recovery-turn-state")
        .header("session-id", format!("recovery-{behavior}-{stream}"))
        .header("thread-id", "same-thread")
        .json(&json!({"model":"mock-model","instructions":behavior,"stream":stream,"input":[
            {"type":"reasoning","id":"rs_recovery","encrypted_content":"PRIVATE_REJECTED_CIPHER","content":null,"summary":[{"type":"summary_text","text":"PRIVATE_SUMMARY"}]},
            {"type":"reasoning","encrypted_content":"PRIVATE_EMPTY_CIPHER","content":null},
            {"type":"compaction","id":"cmp_keep","encrypted_content":"PRIVATE_KEEP_COMPACTION"},
            {"type":"function_call","id":"fc_keep","call_id":"call_keep","name":"test","encrypted_function_args":"PRIVATE_KEEP_ARGS"},
            {"type":"function_call_output","call_id":"call_keep","output":"PRIVATE_TOOL_RESULT"},
            {"role":"user","content":"PRIVATE_PROMPT"}
        ]})).send().await.unwrap()
}

async fn attempted(mock: &Mock, expected: usize) {
    tokio::time::timeout(Duration::from_secs(3), async {
        while mock.calls.load(Ordering::SeqCst) < expected {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
}

async fn headers_recorded(c: &Client, id: &str) -> Value {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let d = c.record(id).await;
            if d["events"]
                .as_array()
                .unwrap()
                .iter()
                .any(|e| e["kind"] == "upstream_attempt_headers")
            {
                return d;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap()
}

pub(super) async fn verify(c: &Client, gateway: &Gateway, mock: &Mock) {
    for stream in [true, false] {
        for (behavior, attempts, success) in [
            ("encrypted-success", 2, true),
            ("encrypted-always", 2, false),
            ("encrypted-wrong-code", 1, false),
            ("encrypted-server-error", 1, false),
        ] {
            let before = mock.calls.load(Ordering::SeqCst);
            let response = request(c, behavior, stream).await;
            let id = response.headers()["x-request-id"]
                .to_str()
                .unwrap()
                .to_owned();
            if stream || success {
                assert_eq!(response.status(), 200);
            }
            let body = response.text().await.unwrap();
            if success {
                assert!(!body.contains("invalid_encrypted_content"));
                assert!(!body.contains("response.failed"));
                assert!(body.contains("completed"));
            }
            let detail = c.record(&id).await;
            let r = &detail["request"];
            assert_eq!(mock.calls.load(Ordering::SeqCst), before + attempts);
            assert_eq!(r["upstream_attempts"], attempts);
            assert_eq!(r["state"], if success { "completed" } else { "failed" });
            assert_eq!(r["binding_generation"], 1);
            assert_eq!(
                r["client_turn_state"]["values"][0]["value"],
                "client-recovery-turn-state"
            );
            assert_eq!(detail["bindings"].as_array().unwrap().len(), 1);
            assert!(!detail.to_string().contains("PRIVATE_"));
            let events = detail["events"].as_array().unwrap();
            let headers: Vec<_> = events
                .iter()
                .filter(|e| e["kind"] == "upstream_attempt_headers")
                .collect();
            assert_eq!(headers.len(), attempts);
            let recovery: Vec<_> = events
                .iter()
                .filter(|e| e["kind"] == "encrypted_reasoning_recovery")
                .collect();
            assert_eq!(recovery.len(), usize::from(attempts == 2));
            if attempts == 2 {
                assert_eq!(headers[0]["details"]["attempt"], 1);
                assert_eq!(headers[0]["details"]["status"], 400);
                assert_eq!(
                    headers[0]["details"]["upstream_request_id"],
                    "mock-rejected-attempt"
                );
                assert_eq!(headers[1]["details"]["attempt"], 2);
                assert_eq!(
                    headers[0]["details"]["turn_state"]["values"][0]["value"],
                    "rejected-turn-state"
                );
                if success {
                    assert!(
                        headers[1]["details"]["turn_state"]["values"][0]["value"]
                            .as_str()
                            .unwrap()
                            .starts_with("server-turn-state-")
                    );
                } else {
                    assert_eq!(
                        headers[1]["details"]["turn_state"]["values"][0]["value"],
                        "rejected-turn-state"
                    );
                }
                assert_eq!(
                    headers[1]["details"]["status"],
                    if success { 200 } else { 400 }
                );
                let cleanup = &recovery[0]["details"]["cleanup"];
                assert_eq!(cleanup["encrypted_fields_removed"], 2);
                assert_eq!(cleanup["null_content_fields_removed"], 2);
                assert_eq!(cleanup["empty_reasoning_items_removed"], 1);
                let captures = mock.captures.lock().await;
                let first = &captures[captures.len() - 2];
                let second = &captures[captures.len() - 1];
                assert!(!first.0.contains_key("x-codex-turn-state"));
                assert!(!second.0.contains_key("x-codex-turn-state"));
                // Same credentials, account, session and thread; only cleaned input differs.
                let mut first_headers = first.0.clone();
                let mut second_headers = second.0.clone();
                first_headers.remove("content-length");
                second_headers.remove("content-length");
                assert_eq!(first_headers, second_headers);
                assert_eq!(
                    second.0["content-length"]
                        .to_str()
                        .unwrap()
                        .parse::<usize>()
                        .unwrap(),
                    serde_json::to_vec(&second.1).unwrap().len()
                );
                let mut expected = first.1.clone();
                let items = expected["input"].as_array_mut().unwrap();
                items.remove(1);
                items[0]
                    .as_object_mut()
                    .unwrap()
                    .remove("encrypted_content");
                items[0].as_object_mut().unwrap().remove("content");
                assert_eq!(second.1, expected);
            }
            if success {
                assert_eq!(r["usage"]["input_tokens"], 1000);
                assert_eq!(r["usage"]["output_tokens"], 500);
                assert_eq!(
                    r["valuation"]["cny"]
                        .as_str()
                        .unwrap()
                        .parse::<rust_decimal::Decimal>()
                        .unwrap(),
                    rust_decimal::Decimal::new(164, 4)
                );
                assert!(r["error_code"].is_null());
                assert!(r["upstream_error"].is_null());
            }
        }
    }

    // Cancellation while the first HTTP 400 body is still arriving cannot send again.
    let before = mock.calls.load(Ordering::SeqCst);
    // Close the real socket; dropping a reqwest response can leave it draining.
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut connection = tokio::net::TcpStream::connect(c.base.trim_start_matches("http://"))
        .await
        .unwrap();
    let payload = json!({"model":"mock-model","instructions":"encrypted-held-error-cancel","stream":true,"input":[{"type":"reasoning","encrypted_content":"PRIVATE_CANCEL_CIPHER"},{"role":"user","content":"PRIVATE_PROMPT"}]}).to_string();
    let wire = format!(
        "POST /v1/responses HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {}\r\nsession-id: recovery-cancel\r\nthread-id: recovery-cancel\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
        c.key,
        payload.len(),
        payload
    );
    connection.write_all(wire.as_bytes()).await.unwrap();
    let mut received = String::new();
    let mut buffer = [0; 4096];
    while !received.contains("\r\n\r\n") {
        let size = connection.read(&mut buffer).await.unwrap();
        assert!(size > 0);
        received.push_str(&String::from_utf8_lossy(&buffer[..size]));
    }
    let id = received
        .lines()
        .find_map(|line| line.strip_prefix("x-request-id: "))
        .unwrap()
        .to_owned();
    headers_recorded(c, &id).await;
    drop(connection);
    wait_inflight(gateway, 0).await;
    mock.release.add_permits(1);
    let detail = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let detail = c.record(&id).await;
            if !detail["request"]["finished_at"].is_null() {
                break detail;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(detail["request"]["state"], "cancelled");
    assert_eq!(detail["request"]["upstream_attempts"], 1);
    assert_eq!(mock.calls.load(Ordering::SeqCst), before + 1);

    // An account disabled after the first rejection is not replaced or resent to.
    let before = mock.calls.load(Ordering::SeqCst);
    let response = request(c, "encrypted-held-error-disabled", true).await;
    let id = response.headers()["x-request-id"]
        .to_str()
        .unwrap()
        .to_owned();
    let detail = headers_recorded(c, &id).await;
    let account = detail["request"]["account_id"].as_str().unwrap();
    c.enabled(account, false).await;
    mock.release.add_permits(1);
    assert!(
        response
            .text()
            .await
            .unwrap()
            .contains("reservation_invalidated")
    );
    let detail = c.record(&id).await;
    assert_eq!(detail["request"]["upstream_attempts"], 1);
    assert_eq!(mock.calls.load(Ordering::SeqCst), before + 1);
    c.enabled(account, true).await;

    // Recovery keeps one capacity slot and the thread lock until it finishes.
    let before = mock.calls.load(Ordering::SeqCst);
    let mut response = request(c, "encrypted-hold-success", true).await;
    let id = response.headers()["x-request-id"]
        .to_str()
        .unwrap()
        .to_owned();
    attempted(mock, before + 2).await;
    response.chunk().await.unwrap();
    wait_inflight(gateway, 1).await;
    let queued = request(c, "encrypted-hold-success", true).await;
    wait_queue(gateway, 1).await;
    assert_eq!(mock.calls.load(Ordering::SeqCst), before + 2);
    mock.release.add_permits(2);
    assert!(
        response
            .text()
            .await
            .unwrap()
            .contains("response.completed")
    );
    assert!(queued.text().await.unwrap().contains("response.completed"));
    assert_eq!(mock.calls.load(Ordering::SeqCst), before + 4);
    assert_eq!(c.record(&id).await["request"]["upstream_attempts"], 2);
    wait_inflight(gateway, 0).await;
}
