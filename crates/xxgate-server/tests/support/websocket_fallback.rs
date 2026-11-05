use super::*;
use tokio::io::AsyncWriteExt;

const CLOSE: usize = 1;
const CREATED_THEN_CLOSE: usize = 2;
const ERROR_EVENT: usize = 3;
const HELD_CLOSE: usize = 4;
const ENCRYPTED_THEN_CLOSE: usize = 5;
const OUTPUT_THEN_CLOSE: usize = 6;
const POLICY_HANDSHAKE: usize = 4000;

pub(super) fn handshake_failure(mock: &Mock) -> Option<Response> {
    let status = mock.handshake_status.swap(0, Ordering::SeqCst);
    if status == POLICY_HANDSHAKE {
        return Some((http::StatusCode::BAD_REQUEST, Json(json!({"error":{"code":"content_policy_violation","message":"test policy rejection"}}))).into_response());
    }
    (status != 0).then(|| {
        (http::StatusCode::from_u16(status as u16).unwrap(), Json(json!({"error":{"code":"unsupported_websocket","message":"test handshake failure"}}))).into_response()
    })
}

pub(super) fn http_failure(mock: &Mock) -> Option<Response> {
    let status = mock.http_status.swap(0, Ordering::SeqCst);
    (status != 0).then(|| {
        let code = if status == 400 {
            "invalid_encrypted_content"
        } else {
            "server_error"
        };
        (
            http::StatusCode::from_u16(status as u16).unwrap(),
            Json(json!({"error":{"code":code,"message":"test HTTP failure"}})),
        )
            .into_response()
    })
}

pub(super) async fn fail_websocket(mock: &Mock, socket: &mut axum::extract::ws::WebSocket) -> bool {
    let mode = mock.ws_failure.swap(0, Ordering::SeqCst);
    let events = match mode {
        0 => return false,
        CLOSE => vec![],
        CREATED_THEN_CLOSE => vec![
            json!({"type":"response.created","response":{"id":"resp_before_close","status":"in_progress"}}),
        ],
        OUTPUT_THEN_CLOSE => {
            vec![json!({"type":"response.output_text.delta","delta":"PRIVATE_ALREADY_DELIVERED"})]
        }
        ERROR_EVENT => vec![
            json!({"type":"error","error":{"code":"rate_limit_exceeded","message":"test quota rejection"}}),
        ],
        HELD_CLOSE => {
            mock.release_failure.notified().await;
            vec![]
        }
        ENCRYPTED_THEN_CLOSE => {
            mock.ws_failure.store(CLOSE, Ordering::SeqCst);
            vec![
                json!({"type":"response.failed","response":{"id":"resp_encrypted_rejected","status":"failed","error":{"code":"invalid_encrypted_content","message":"Encrypted content could not be decrypted or parsed."}}}),
            ]
        }
        _ => panic!("unknown WS failure mode"),
    };
    for event in events {
        let _ = socket.send(Message::Text(event.to_string().into())).await;
    }
    let _ = socket.send(Message::Close(None)).await;
    true
}

fn body(stream: bool) -> Value {
    json!({"model":"ws-test","stream":stream,"input":[
        {"type":"reasoning","id":"rs_original","encrypted_content":"PRIVATE_CIPHER","summary":[]},
        {"type":"function_call","id":"fc_original","call_id":"call_original","name":"test","arguments":"{}"},
        {"type":"function_call_output","call_id":"call_original","output":"PRIVATE_RESULT"},
        {"role":"user","content":"PRIVATE_FALLBACK_INPUT"}
    ],"client_metadata":{"turn_id":"fallback-turn"}})
}

async fn request(
    client: &reqwest::Client,
    base: &str,
    key: &str,
    body: &Value,
) -> reqwest::Response {
    client
        .post(format!("{base}/v1/responses"))
        .bearer_auth(key)
        .header("session-id", format!("fallback-{}", Uuid::new_v4()))
        .json(body)
        .send()
        .await
        .unwrap()
}

async fn detail(
    client: &reqwest::Client,
    base: &str,
    response: reqwest::Response,
) -> (String, Value) {
    let id = response.headers()["x-request-id"]
        .to_str()
        .unwrap()
        .to_owned();
    let text = response.text().await.unwrap();
    let detail = admin(client, base, &format!("/requests/{id}"), "GET", None).await;
    (text, detail)
}

async fn wait_for_capture(mock: &Mock, count: usize) {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while mock.captures.lock().await.len() < count {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
}

pub(super) async fn verify(
    client: &reqwest::Client,
    base: &str,
    key: &str,
    gateway: &Gateway,
    mock: &Mock,
) {
    // Retry the original full HTTP document; do not strip encrypted/tool history.
    for stream in [false, true] {
        let before = mock.http_requests.load(Ordering::SeqCst);
        let mut sent = body(stream);
        if !stream {
            sent["input"][3]["content"] = json!("x".repeat(18 * 1024 * 1024));
        }
        mock.ws_failure.store(CLOSE, Ordering::SeqCst);
        let (text, d) = detail(client, base, request(client, base, key, &sent).await).await;
        assert!(
            text.contains("completed") && !text.contains("response.failed"),
            "{text}"
        );
        assert_eq!(mock.http_requests.load(Ordering::SeqCst), before + 1);
        assert_eq!(d["request"]["state"], "completed");
        assert_eq!(d["request"]["upstream_attempts"], 2);
        assert_eq!(d["request"]["upstream_transport"], "http");
        assert_eq!(d["request"]["usage"]["output_tokens"], 1);
        assert_ne!(d["request"]["detail_level"], "summary");
        let events = d["events"].as_array().unwrap();
        let fallback = events
            .iter()
            .find(|e| e["kind"] == "websocket_http_fallback")
            .unwrap();
        assert_eq!(fallback["details"]["reason"], "websocket_transport_failed");
        assert_eq!(
            fallback["details"]["binding_id"],
            d["request"]["binding_id"]
        );
        let attempts: Vec<_> = events
            .iter()
            .filter(|e| e["kind"] == "upstream_attempt_headers")
            .collect();
        // A large rejected WS frame may fail during send, before response headers.
        assert!((1..=2).contains(&attempts.len()));
        if attempts.len() == 2 {
            assert_eq!(attempts[0]["details"]["transport"], "websocket");
        }
        assert_eq!(attempts.last().unwrap()["details"]["transport"], "http");
        assert_eq!(attempts.last().unwrap()["details"]["attempt"], 2);
        assert!(!d.to_string().contains("PRIVATE_"));
        let captures = mock.http_captures.lock().await;
        let (headers, forwarded) = captures.last().unwrap();
        assert!(forwarded["input"] == sent["input"]);
        assert!(forwarded.get("previous_response_id").is_none());
        assert!(forwarded.get("type").is_none());
        assert_eq!(headers["chatgpt-account-id"], "test-only");
        assert_eq!(headers["authorization"], "Bearer synthetic-token");
    }

    // After WS incrementalization fails, HTTP must still receive full history.
    let session = format!("fallback-incremental-{}", Uuid::new_v4());
    let mut sent = body(false);
    let first: Value = client
        .post(format!("{base}/v1/responses"))
        .bearer_auth(key)
        .header("session-id", &session)
        .json(&sent)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(first["status"], "completed");
    let input = sent["input"].as_array_mut().unwrap();
    input.extend(first["output"].as_array().unwrap().iter().cloned());
    input.push(json!({"role":"user","content":"PRIVATE_INCREMENTAL_TURN"}));
    mock.ws_failure.store(CLOSE, Ordering::SeqCst);
    let response = client
        .post(format!("{base}/v1/responses"))
        .bearer_auth(key)
        .header("session-id", &session)
        .json(&sent)
        .send()
        .await
        .unwrap();
    let (text, d) = detail(client, base, response).await;
    assert!(text.contains("completed"), "{text}");
    assert_eq!(d["request"]["upstream_attempts"], 2);
    {
        let captures = mock.captures.lock().await;
        let incremental = captures.last().unwrap();
        assert_eq!(incremental["input"].as_array().unwrap().len(), 1);
        assert_eq!(incremental["previous_response_id"], first["id"]);
        let http = mock.http_captures.lock().await;
        let forwarded = &http.last().unwrap().1;
        assert!(forwarded["input"] == sent["input"]);
        assert!(forwarded.get("previous_response_id").is_none());
    }

    // Both an HTTP handshake rejection and an invalid 200 upgrade can use HTTP.
    for status in [400, 404, 405, 426, 503, 200] {
        mock.handshake_status.store(status, Ordering::SeqCst);
        let before = mock.http_requests.load(Ordering::SeqCst);
        let (text, d) = detail(client, base, request(client, base, key, &body(false)).await).await;
        assert!(text.contains("completed"), "handshake {status}: {text}");
        assert_eq!(mock.http_requests.load(Ordering::SeqCst), before + 1);
        assert_eq!(d["request"]["upstream_attempts"], 2);
        assert_eq!(d["request"]["upstream_transport"], "http");
    }

    // A silent WS timeout can fall back while the original client is connected.
    let settings = gateway.settings.current();
    let mut short = settings.clone();
    short.sse_idle_timeout_ms = 200;
    gateway.settings.publish(short);
    mock.ws_failure.store(HELD_CLOSE, Ordering::SeqCst);
    let (text, d) = detail(client, base, request(client, base, key, &body(false)).await).await;
    gateway.settings.publish(settings);
    mock.release_failure.notify_one();
    assert!(text.contains("completed"), "{text}");
    assert_eq!(d["request"]["upstream_attempts"], 2);
    let reason = d["events"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["kind"] == "websocket_http_fallback")
        .unwrap();
    assert_eq!(reason["details"]["reason"], "upstream_idle_timeout");

    // Any observed event prevents fallback, including a buffered response.created.
    for mode in [CREATED_THEN_CLOSE, OUTPUT_THEN_CLOSE, ERROR_EVENT] {
        mock.ws_failure.store(mode, Ordering::SeqCst);
        let before = mock.http_requests.load(Ordering::SeqCst);
        let (_, d) = detail(client, base, request(client, base, key, &body(true)).await).await;
        assert_eq!(d["request"]["state"], "failed", "{d}");
        assert_eq!(d["request"]["upstream_attempts"], 1);
        assert_eq!(d["request"]["upstream_transport"], "websocket");
        assert_eq!(mock.http_requests.load(Ordering::SeqCst), before);
        assert!(
            !d["events"]
                .as_array()
                .unwrap()
                .iter()
                .any(|e| e["kind"] == "websocket_http_fallback")
        );
    }
    mock.handshake_status.store(429, Ordering::SeqCst);
    let before = mock.http_requests.load(Ordering::SeqCst);
    let (_, d) = detail(client, base, request(client, base, key, &body(false)).await).await;
    assert_eq!(d["request"]["upstream_attempts"], 1);
    assert_eq!(mock.http_requests.load(Ordering::SeqCst), before);

    // A policy rejection remains final even for requests with no session identity.
    for stateless in [false, true] {
        mock.handshake_status
            .store(POLICY_HANDSHAKE, Ordering::SeqCst);
        let before = mock.http_requests.load(Ordering::SeqCst);
        let response = if stateless {
            client
                .post(format!("{base}/v1/responses"))
                .bearer_auth(key)
                .json(&body(false))
                .send()
                .await
                .unwrap()
        } else {
            request(client, base, key, &body(false)).await
        };
        let (_, d) = detail(client, base, response).await;
        assert_eq!(
            d["request"]["error_code"],
            "upstream_content_policy_violation"
        );
        assert_eq!(d["request"]["upstream_attempts"], 1);
        assert_eq!(mock.http_requests.load(Ordering::SeqCst), before);
    }

    // HTTP failure stays final, including encrypted rejection after WS fallback.
    for status in [503, 400] {
        mock.ws_failure.store(CLOSE, Ordering::SeqCst);
        mock.http_status.store(status, Ordering::SeqCst);
        let before = mock.http_requests.load(Ordering::SeqCst);
        let (_, d) = detail(client, base, request(client, base, key, &body(false)).await).await;
        assert_eq!(d["request"]["state"], "failed");
        assert_eq!(d["request"]["upstream_attempts"], 2);
        assert_eq!(d["request"]["upstream_transport"], "http");
        assert_eq!(mock.http_requests.load(Ordering::SeqCst), before + 1);
        assert!(
            !d["events"]
                .as_array()
                .unwrap()
                .iter()
                .any(|e| e["kind"] == "encrypted_reasoning_recovery")
        );
    }
    mock.ws_failure
        .store(ENCRYPTED_THEN_CLOSE, Ordering::SeqCst);
    let before = mock.http_requests.load(Ordering::SeqCst);
    let (_, d) = detail(client, base, request(client, base, key, &body(false)).await).await;
    assert_eq!(d["request"]["upstream_attempts"], 2);
    assert_eq!(d["request"]["upstream_transport"], "websocket");
    assert_eq!(mock.http_requests.load(Ordering::SeqCst), before);

    // Native downstream WS never changes protocol, even before its first event.
    let mut native = connect(base, key).await;
    mock.ws_failure.store(CLOSE, Ordering::SeqCst);
    let before = mock.http_requests.load(Ordering::SeqCst);
    let result = exchange(
        &mut native,
        json!({"type":"response.create","model":"ws-test","input":[]}),
    )
    .await;
    assert_eq!(
        result["response"]["error"]["code"],
        "websocket_transport_failed"
    );
    assert_eq!(mock.http_requests.load(Ordering::SeqCst), before);
    native.close(None).await.unwrap();

    // A revoked Key must block the retry even after WS has already accepted work.
    let keys = admin(client, base, "/keys", "GET", None).await;
    let id = keys["items"][0]["id"].as_str().unwrap();
    let captures = mock.captures.lock().await.len();
    mock.ws_failure.store(HELD_CLOSE, Ordering::SeqCst);
    let response = request(client, base, key, &body(true)).await;
    wait_for_capture(mock, captures + 1).await;
    admin(
        client,
        base,
        &format!("/keys/{id}/enabled"),
        "PUT",
        Some(json!({"enabled":false})),
    )
    .await;
    mock.release_failure.notify_one();
    let (_, d) = detail(client, base, response).await;
    assert_eq!(d["request"]["upstream_attempts"], 1);
    assert_eq!(mock.http_requests.load(Ordering::SeqCst), before);
    admin(
        client,
        base,
        &format!("/keys/{id}/enabled"),
        "PUT",
        Some(json!({"enabled":true})),
    )
    .await;

    // Closing an HTTP socket while WS is silent must not start HTTP fallback.
    let address = url::Url::parse(base).unwrap();
    let mut socket =
        tokio::net::TcpStream::connect((address.host_str().unwrap(), address.port().unwrap()))
            .await
            .unwrap();
    let payload = body(true).to_string();
    let wire = format!(
        "POST /v1/responses HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {key}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
        address.host_str().unwrap(),
        payload.len()
    );
    let captures = mock.captures.lock().await.len();
    mock.ws_failure.store(HELD_CLOSE, Ordering::SeqCst);
    socket.write_all(wire.as_bytes()).await.unwrap();
    wait_for_capture(mock, captures + 1).await;
    drop(socket);
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while gateway.scheduler.stats().inflight != 0 {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    mock.release_failure.notify_one();
    assert_eq!(mock.http_requests.load(Ordering::SeqCst), before);
}
