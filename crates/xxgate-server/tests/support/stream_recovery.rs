use super::*;

const TOOL_ERROR: &str = "Encrypted function output content could not be decrypted or decoded.";
fn input() -> Value {
    json!([
        {"type":"reasoning","id":"rs_keep","encrypted_content":"PRIVATE_REASONING","summary":[]},
        {"type":"compaction","id":"cmp_keep","encrypted_content":"PRIVATE_COMPACTION"},
        {"type":"function_call","id":"fc_keep","call_id":"call_keep","name":"test","arguments":"{}","encrypted_function_args":["PRIVATE_ARGS"]},
        {"type":"function_call_output","id":"fco_keep","call_id":"call_keep","output":[{"type":"input_text","text":"PRIVATE_PLAIN_RESULT"},{"type":"encrypted_content","encrypted_content":"PRIVATE_BAD_TOOL_RESULT"}]},
        {"type":"custom_tool_call","call_id":"call_custom","name":"test","input":"PRIVATE_CUSTOM_INPUT"},
        {"type":"custom_tool_call_output","call_id":"call_custom","output":[{"type":"encrypted_content","encrypted_content":"PRIVATE_ONLY_CIPHER"}]},
        {"role":"user","content":"PRIVATE_PROMPT"}
    ])
}
fn payload(behavior: &str, stream: bool) -> Value {
    let mut input = input();
    if behavior == "stream-recovery-reasoning" {
        input = json!([{"type":"reasoning","id":"rs_keep","encrypted_content":"PRIVATE_REASONING","summary":[]},{"role":"user","content":"PRIVATE_PROMPT"}]);
    }
    if behavior == "stream-recovery-no-parts" {
        input = json!([{"role":"user","content":"PRIVATE_PROMPT"}]);
    }
    json!({"model":"mock-model","instructions":behavior,"stream":stream,"input":input})
}

pub(super) fn upstream(mock: &Mock, body: &Value, behavior: &str) -> Option<Response> {
    if !behavior.starts_with("stream-recovery-") {
        return None;
    }
    let encrypted = body["input"].as_array().unwrap().iter().any(|i| {
        if behavior == "stream-recovery-reasoning" {
            i["type"] == "reasoning" && i.get("encrypted_content").is_some()
        } else {
            i["output"]
                .as_array()
                .is_some_and(|p| p.iter().any(|v| v["type"] == "encrypted_content"))
        }
    });
    if !encrypted
        && !matches!(
            behavior,
            "stream-recovery-always"
                | "stream-recovery-http-sse"
                | "stream-recovery-sse-http"
                | "stream-recovery-no-parts"
        )
    {
        return None;
    }
    let message = if behavior == "stream-recovery-reasoning" {
        "Encrypted content could not be decrypted or parsed."
    } else {
        TOOL_ERROR
    };
    let wrong = behavior == "stream-recovery-wrong-code";
    let code = if wrong {
        "invalid_request_error"
    } else {
        "invalid_encrypted_content"
    };
    if behavior == "stream-recovery-http"
        || (behavior == "stream-recovery-http-sse" && encrypted)
        || (behavior == "stream-recovery-sse-http" && !encrypted)
    {
        return Some(
            (
                StatusCode::BAD_REQUEST,
                Json(json!({"error":{"code":code,"message":message}})),
            )
                .into_response(),
        );
    }
    let mut events = vec![
        json!({"type":"response.created","response":{"id":"resp_rejected_hidden","status":"in_progress","output":[]}}),
    ];
    if behavior == "stream-recovery-prefix-limit" {
        for _ in 0..17 {
            events.push(events[0].clone());
        }
    }
    if behavior == "stream-recovery-tool-started" {
        events.push(json!({"type":"response.output_item.added","item":{"type":"function_call","id":"fc_started","call_id":"call_started","name":"test","arguments":""},"output_index":0}));
    }
    if behavior == "stream-recovery-output" {
        events.push(json!({"type":"response.output_text.delta","delta":"PRIVATE_ALREADY_DELIVERED","item_id":"msg_started","output_index":0,"content_index":0}));
    }
    if behavior == "stream-recovery-unknown-event" {
        events.push(json!({"type":"response.future_event","future":true}));
    }
    let mut terminal = json!({"type":"response.failed","response":{"id":"resp_rejected_hidden","status":"failed","output":[],"error":{"code":code,"message":message}}});
    if behavior == "stream-recovery-terminal-output" {
        terminal["response"]["output"] = json!([{"type":"message","content":[{"type":"output_text","text":"PRIVATE_TERMINAL_OUTPUT"}]}]);
    }
    if behavior == "stream-recovery-usage" {
        terminal["response"]["usage"] = json!({"input_tokens":20,"output_tokens":1});
    }
    if behavior == "stream-recovery-top-error" {
        terminal = json!({"type":"error","code":code,"message":message});
    }
    if behavior == "stream-recovery-overload" {
        terminal["response"]["error"] = json!({"code":"server_error","message":"Our servers are currently overloaded. Please try again later."});
    }
    events.push(terminal);
    let mock = mock.clone();
    let held = matches!(
        behavior,
        "stream-recovery-held" | "stream-recovery-cancel" | "stream-recovery-idle"
    );
    let (tx, rx) = mpsc::channel(1);
    tokio::spawn(async move {
        for (i, event) in events.into_iter().enumerate() {
            if held && i == 1 {
                let permit = mock.release.acquire().await.unwrap();
                permit.forget();
            }
            // Separate preamble and rejection frames across real HTTP chunks.
            if tx.send(Ok::<_, Infallible>(frame(event))).await.is_err() {
                return;
            }
            tokio::task::yield_now().await;
        }
    });
    Some(
        (
            [
                ("content-type", "text/event-stream"),
                ("x-codex-turn-state", "rejected-stream-state"),
            ],
            Body::from_stream(ReceiverStream::new(rx)),
        )
            .into_response(),
    )
}
async fn send(c: &Client, behavior: &str, stream: bool) -> reqwest::Response {
    c.http
        .post(format!("{}/v1/responses", c.base))
        .bearer_auth(&c.key)
        .header("session-id", format!("{behavior}-{stream}"))
        .header("thread-id", "stream-thread")
        .header("x-codex-turn-state", "client-state-still-dropped")
        .json(&payload(behavior, stream))
        .send()
        .await
        .unwrap()
}
async fn observed(c: &Client, id: &str) -> Value {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let d = c.record(id).await;
            if d["events"]
                .as_array()
                .unwrap()
                .iter()
                .any(|e| e["kind"] == "upstream_attempt_headers")
            {
                break d;
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
            ("stream-recovery-success", 2, true),
            ("stream-recovery-reasoning", 2, true),
            ("stream-recovery-http", 2, true),
            ("stream-recovery-top-error", 2, true),
            ("stream-recovery-always", 2, false),
            ("stream-recovery-http-sse", 2, false),
            ("stream-recovery-sse-http", 2, false),
            ("stream-recovery-tool-started", 1, false),
            ("stream-recovery-output", 1, false),
            ("stream-recovery-terminal-output", 1, false),
            ("stream-recovery-usage", 1, false),
            ("stream-recovery-prefix-limit", 1, false),
            ("stream-recovery-unknown-event", 1, false),
            ("stream-recovery-wrong-code", 1, false),
            ("stream-recovery-no-parts", 1, false),
            ("stream-recovery-overload", 1, false),
        ] {
            let before = mock.calls.load(Ordering::SeqCst);
            let response = send(c, behavior, stream).await;
            let id = response.headers()["x-request-id"]
                .to_str()
                .unwrap()
                .to_owned();
            assert!(!response.headers().contains_key("x-codex-turn-state"));
            let body = response.text().await.unwrap();
            let d = c.record(&id).await;
            let r = &d["request"];
            assert_eq!(
                mock.calls.load(Ordering::SeqCst),
                before + attempts,
                "{behavior}"
            );
            assert_eq!(r["upstream_attempts"], attempts, "{behavior}");
            assert_eq!(
                r["state"],
                if success { "completed" } else { "failed" },
                "{behavior}"
            );
            assert!(!d.to_string().contains("PRIVATE_"));
            if success {
                assert!(!body.contains("resp_rejected_hidden"));
                assert!(!body.contains("response.failed"));
                if stream {
                    assert_eq!(body.matches("event: response.created").count(), 1);
                }
                assert_eq!(r["usage"]["input_tokens"], 1000);
                assert_eq!(r["usage"]["output_tokens"], 500);
                assert!(r["error_code"].is_null());
                assert_eq!(
                    r["valuation"]["cny"]
                        .as_str()
                        .unwrap()
                        .parse::<rust_decimal::Decimal>()
                        .unwrap(),
                    rust_decimal::Decimal::new(164, 4)
                );
            } else if stream {
                assert_eq!(body.matches("event: response.failed").count(), 1);
            }
            let recoveries: Vec<_> = d["events"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|e| e["kind"] == "encrypted_reasoning_recovery")
                .collect();
            assert_eq!(recoveries.len(), usize::from(attempts == 2));
            if attempts == 2 {
                let facts = &recoveries[0]["details"];
                assert_eq!(
                    facts["source"],
                    if matches!(
                        behavior,
                        "stream-recovery-http" | "stream-recovery-http-sse"
                    ) {
                        "http_error"
                    } else {
                        "sse_error"
                    }
                );
                let reasoning = behavior == "stream-recovery-reasoning";
                assert_eq!(
                    facts["cleanup"]["encrypted_tool_parts_replaced"],
                    if reasoning { 0 } else { 2 }
                );
                assert_eq!(
                    facts["cleanup"]["tool_outputs_changed"],
                    if reasoning { 0 } else { 2 }
                );
                assert_eq!(
                    facts["cleanup"]["encrypted_fields_removed"],
                    if reasoning { 1 } else { 0 }
                );
                let captures = mock.captures.lock().await;
                let first = &captures[captures.len() - 2];
                let second = &captures[captures.len() - 1];
                for name in [
                    "authorization",
                    "chatgpt-account-id",
                    "session-id",
                    "thread-id",
                    "x-codex-turn-metadata",
                ] {
                    assert_eq!(first.0[name], second.0[name]);
                }
                assert!(!second.0.contains_key("x-codex-turn-state"));
                let mut expected = first.1.clone();
                if reasoning {
                    expected["input"][0]
                        .as_object_mut()
                        .unwrap()
                        .remove("encrypted_content");
                }
                for index in if reasoning { vec![] } else { vec![3, 5] } {
                    let part = if index == 3 { 1 } else { 0 };
                    let marker = second.1["input"][index]["output"][part].clone();
                    assert_eq!(marker["type"], "input_text");
                    assert!(
                        marker["text"]
                            .as_str()
                            .unwrap()
                            .contains("historical tool result")
                    );
                    expected["input"][index]["output"][part] = marker;
                }
                assert_eq!(second.1, expected);
            }
            if behavior == "stream-recovery-overload" {
                assert_eq!(r["upstream_error"]["reason"], "upstream_overloaded");
            }
            if behavior == "stream-recovery-usage" {
                assert_eq!(r["usage"]["output_tokens"], 1);
            }
        }
    }

    // A control preamble is not delivered while waiting for the rejection.
    let before = mock.calls.load(Ordering::SeqCst);
    let mut response = send(c, "stream-recovery-held", true).await;
    let id = response.headers()["x-request-id"]
        .to_str()
        .unwrap()
        .to_owned();
    let d = observed(c, &id).await;
    let heartbeat = response.chunk().await.unwrap().unwrap();
    assert!(heartbeat.starts_with(b": xxgate.queue_heartbeat"));
    let account = d["request"]["account_id"].as_str().unwrap();
    c.enabled(account, false).await;
    mock.release.add_permits(1);
    let body = response.text().await.unwrap();
    assert!(!body.contains("resp_rejected_hidden"));
    assert!(body.contains("reservation_invalidated"));
    assert_eq!(mock.calls.load(Ordering::SeqCst), before + 1);
    c.enabled(account, true).await;
    wait_inflight(gateway, 0).await;

    // A real downstream disconnect while the preamble is buffered cancels the
    // request and releases its capacity without attempting recovery.
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let before = mock.calls.load(Ordering::SeqCst);
    let mut socket = tokio::net::TcpStream::connect(c.base.trim_start_matches("http://"))
        .await
        .unwrap();
    let body = payload("stream-recovery-cancel", true).to_string();
    let wire = format!(
        "POST /v1/responses HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {}\r\nsession-id: stream-recovery-cancel\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
        c.key,
        body.len(),
        body
    );
    socket.write_all(wire.as_bytes()).await.unwrap();
    let mut received = String::new();
    let mut bytes = [0; 4096];
    while !received.contains("\r\n\r\n") {
        let n = socket.read(&mut bytes).await.unwrap();
        assert!(n > 0);
        received.push_str(&String::from_utf8_lossy(&bytes[..n]));
    }
    let id = received
        .lines()
        .find_map(|l| l.strip_prefix("x-request-id: "))
        .unwrap()
        .to_owned();
    observed(c, &id).await;
    drop(socket);
    wait_inflight(gateway, 0).await;
    mock.release.add_permits(1);
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let d = c.record(&id).await;
            if d["request"]["state"] == "cancelled" {
                assert_eq!(d["request"]["upstream_attempts"], 1);
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(mock.calls.load(Ordering::SeqCst), before + 1);

    // Heartbeats cannot extend the upstream idle deadline or trigger a resend.
    let original = c.admin("/settings", "GET", None).await;
    let mut cfg = original.clone();
    cfg["sse_idle_timeout_ms"] = json!(120);
    cfg["heartbeat_interval_ms"] = json!(50);
    c.admin("/settings", "PUT", Some(cfg)).await;
    let before = mock.calls.load(Ordering::SeqCst);
    let response = send(c, "stream-recovery-idle", true).await;
    let body = response.text().await.unwrap();
    assert!(body.contains("upstream_idle_timeout"));
    assert!(!body.contains("resp_rejected_hidden"));
    assert_eq!(mock.calls.load(Ordering::SeqCst), before + 1);
    mock.release.add_permits(1);
    let mut restore = c.admin("/settings", "GET", None).await;
    restore["sse_idle_timeout_ms"] = original["sse_idle_timeout_ms"].clone();
    restore["heartbeat_interval_ms"] = original["heartbeat_interval_ms"].clone();
    c.admin("/settings", "PUT", Some(restore)).await;
    wait_inflight(gateway, 0).await;
}
