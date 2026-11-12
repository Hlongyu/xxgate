//! End-to-end HTTP -> pooled WS -> decoder -> PostgreSQL -> HTTP/SSE contract.
use axum::{
    Json, Router,
    extract::{State, WebSocketUpgrade, ws::Message},
    response::{IntoResponse, Response},
    routing::get,
};
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use tokio::{sync::Mutex, task::JoinHandle};
use tokio_tungstenite::{
    WebSocketStream,
    tungstenite::{Message as ClientMessage, client::IntoClientRequest},
};
use uuid::Uuid;
use xxgate_codex::{ingress::ResponsesIngress, provider::CodexProvider};
use xxgate_core::{
    access::{CredentialCipher, hash_password},
    application::{gateway::Gateway, ports::AccessStore},
    memory::{MemoryDomain, MemoryProbe, MemorySample},
};
use xxgate_postgres::PgStore;
use xxgate_server::http::{AppState, router};
use xxgate_transport::HttpTransport;

#[path = "support/websocket_fallback.rs"]
mod fallback_contract;

// Transport contracts must not depend on concurrent memory use on the test host.
// Admission and memory-pressure behavior are exercised by the memory contract.
struct HealthyMemory;
impl MemoryProbe for HealthyMemory {
    fn sample(&self) -> Result<MemorySample, &'static str> {
        Ok(MemorySample {
            domains: vec![MemoryDomain {
                source: "test".into(),
                limit_bytes: 1_000_000,
                used_bytes: 1,
                available_bytes: 999_999,
            }],
        })
    }
}

#[derive(Clone, Default)]
struct Mock {
    connections: Arc<AtomicUsize>,
    captures: Arc<Mutex<Vec<Value>>>,
    http_requests: Arc<AtomicUsize>,
    close_after: Arc<AtomicUsize>,
    stall_next: Arc<AtomicUsize>,
    ws_failure: Arc<AtomicUsize>,
    handshake_status: Arc<AtomicUsize>,
    http_status: Arc<AtomicUsize>,
    release_failure: Arc<tokio::sync::Notify>,
    http_captures: Arc<Mutex<Vec<(http::HeaderMap, Value)>>>,
}

async fn upstream(ws: WebSocketUpgrade, State(mock): State<Mock>) -> Response {
    mock.connections.fetch_add(1, Ordering::SeqCst);
    if let Some(response) = fallback_contract::handshake_failure(&mock) {
        return response;
    }
    ws.on_upgrade(move |mut socket| async move {
        let mut last_response_id = String::new();
        while let Some(Ok(message)) = socket.recv().await {
            let Message::Text(text) = message else { continue; };
            let request: Value = serde_json::from_str(&text).unwrap();
            assert_eq!(request["type"], "response.create");
            assert!(request.get("stream").is_none());
            let prewarm = request["generate"] == false;
            let mut captures = mock.captures.lock().await;
            if captures.len() == 2 {
                assert_eq!(request["previous_response_id"], last_response_id);
                assert_eq!(request["input"].as_array().unwrap().len(), 1);
                assert_eq!(request["input"][0]["content"], "PRIVATE_WS_INPUT_3");
            } else if request.get("previous_response_id").is_some_and(|v| !v.is_null()) {
                assert_eq!(request["previous_response_id"], last_response_id);
            }
            captures.push(request);
            drop(captures);
            if fallback_contract::fail_websocket(&mock, &mut socket).await { return; }
            if mock.stall_next.swap(0, Ordering::SeqCst) == 1 {
                socket.send(Message::Text(json!({"type":"response.created","response":{"id":"resp_stalled","status":"in_progress"}}).to_string().into())).await.unwrap();
                while socket.recv().await.is_some() {}
                return;
            }
            let id = format!("resp_{}", Uuid::new_v4().simple());
            let output = if prewarm { json!([]) } else { json!([{"id":"msg_test","type":"message","role":"assistant","status":"completed","content":[{"type":"output_text","text":"WS answer","annotations":[]}]}]) };
            let usage = if prewarm { json!({"input_tokens":0,"output_tokens":0}) } else { json!({"input_tokens":12,"input_tokens_details":{"cached_tokens":4},"output_tokens":3,"output_tokens_details":{"reasoning_tokens":1}}) };
            for event in [
                json!({"type":"response.created","response":{"id":id,"model":"ws-model","status":"in_progress"}}),
                json!({"type":"response.output_text.delta","delta":"WS answer"}),
                json!({"type":"response.completed","response":{"id":id,"object":"response","model":"ws-model","status":"completed","output":output,"usage":usage}}),
            ] {
                if prewarm && event["type"] == "response.output_text.delta" { continue; }
                if socket.send(Message::Text(event.to_string().into())).await.is_err() { return; }
            }
            last_response_id = id;
            if mock.close_after.swap(0, Ordering::SeqCst) == 1 { return; }
        }
    }).into_response()
}

async fn serve(app: Router) -> (String, JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    (
        url,
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        }),
    )
}

async fn admin(
    client: &reqwest::Client,
    base: &str,
    path: &str,
    method: &str,
    body: Option<Value>,
) -> Value {
    let mut request = client
        .request(method.parse().unwrap(), format!("{base}/api/admin{path}"))
        .header("x-xxgate-csrf", "1");
    if let Some(body) = body {
        request = request.json(&body);
    }
    let response = request.send().await.unwrap();
    let status = response.status();
    let body: Value = response.json().await.unwrap();
    assert!(status.is_success(), "{path}: {status} {body}");
    body
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database; run scripts/test.py websocket_gateway_contract"]
async fn websocket_gateway_contract() {
    // Isolate from other integration tests even when sharing a disposable DB.
    let database = std::env::var("TEST_DATABASE_URL").expect("disposable database required");
    let pool = sqlx::PgPool::connect(&database).await.unwrap();
    let schema = format!("ws_{}", Uuid::new_v4().simple());
    sqlx::query(&format!("CREATE SCHEMA {schema}"))
        .execute(&pool)
        .await
        .unwrap();
    let mut scoped = url::Url::parse(&database).unwrap();
    scoped
        .query_pairs_mut()
        .append_pair("options", &format!("-csearch_path={schema}"));
    let store = Arc::new(PgStore::connect(scoped.as_str()).await.unwrap());
    store
        .initialize_admin(&hash_password("websocket-test-password").unwrap())
        .await
        .unwrap();
    let mock = Mock::default();
    let (upstream_url, upstream_task) = serve(
        Router::new()
            .route("/responses", get(upstream).post(http_upstream))
            .route(
                "/models",
                get(|| async {
                    Json(json!({"models":[{"slug":"ws-model","display_name":"WS simulation"}]}))
                }),
            )
            .layer(axum::extract::DefaultBodyLimit::max(32 * 1024 * 1024))
            .with_state(mock.clone()),
    )
    .await;
    let gateway = Gateway::new_with_memory_probe(
        store.clone(),
        CredentialCipher::new(&[7; 32]),
        Arc::new(ResponsesIngress),
        Arc::new(CodexProvider),
        |settings| Arc::new(HttpTransport::new(settings, true)),
        Arc::new(HealthyMemory),
    )
    .await
    .unwrap();
    let (base, task) = serve(router(AppState::new(gateway.clone(), true, false))).await;
    let client = reqwest::Client::builder()
        .no_proxy()
        .cookie_store(true)
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .unwrap();
    admin(
        &client,
        &base,
        "/login",
        "POST",
        Some(json!({"password":"websocket-test-password"})),
    )
    .await;
    let key = admin(
        &client,
        &base,
        "/keys",
        "POST",
        Some(json!({"name":"WS test"})),
    )
    .await["secret"]
        .as_str()
        .unwrap()
        .to_owned();
    admin(&client, &base, "/models", "PUT", Some(json!({"id":"ws-test","upstream":{"provider":"openai","access_kind":"codex_oauth","model":"ws-model"},"enabled":true,"capabilities":{},"version":0}))).await;
    let account = admin(&client, &base, "/accounts", "POST", Some(json!({"name":"WS simulation","responses_websocket_enabled":true,"upstream_account_id":"test-only","upstream_base_url":upstream_url,"max_inflight":2,"credentials":{"access_token":"synthetic-token","refresh_token":"","expires_at":null}}))).await;
    let account_id = account["id"].as_str().unwrap();
    gateway
        .sync_models(Uuid::parse_str(account_id).unwrap())
        .await
        .unwrap();
    admin(
        &client,
        &base,
        &format!("/accounts/{account_id}/enabled"),
        "PUT",
        Some(json!({"enabled":true})),
    )
    .await;
    let mut binding = Value::Null;
    let mut previous_output = Vec::new();
    let mut previous_response_id = String::new();
    let mut ws_request_ids = Vec::new();
    for (turn, stream) in [(1, true), (2, false), (3, true)] {
        // Inbound headers contain ASCII JSON escapes. Identity rewriting decodes
        // these to UTF-8 before the transport builds each WS create event.
        let label = match turn {
            1 => "ASCII",
            2 => r"\u4e2d\u6587",
            _ => r"\ud83d\ude00",
        };
        let turn_metadata =
            format!(r#"{{"turn_id":"turn-{turn}","tool_namespaces_info":"{label}"}}"#);
        let mut input = Vec::new();
        if turn == 3 {
            input.push(json!({"role":"user","content":"PRIVATE_WS_INPUT_2"}));
            input.extend(previous_output.clone());
        }
        input.push(json!({"role":"user","content":format!("PRIVATE_WS_INPUT_{turn}")}));
        let response = client.post(format!("{base}/v1/responses")).bearer_auth(&key)
            .header("session-id", "ws-session").header("thread-id", "ws-thread")
            .header("x-codex-turn-metadata", &turn_metadata)
            .json(&json!({"model":"ws-test","input":input,"stream":stream,"client_metadata":{"turn_id":format!("turn-{turn}"),"x-codex-turn-metadata":turn_metadata}}))
            .send().await.unwrap();
        if response.status() != 200 {
            panic!(
                "unexpected status {}: {}",
                response.status(),
                response.text().await.unwrap()
            );
        }
        let request_id = response.headers()["x-request-id"]
            .to_str()
            .unwrap()
            .to_owned();
        if stream {
            let body = response.text().await.unwrap();
            assert!(
                body.contains("response.completed") && body.contains("WS answer"),
                "{body}"
            );
        } else {
            let body: Value = response.json().await.unwrap();
            assert_eq!(body["status"], "completed");
            assert_eq!(body["output"][0]["content"][0]["text"], "WS answer");
            previous_output = body["output"].as_array().unwrap().clone();
            previous_response_id = body["id"].as_str().unwrap().to_owned();
        }
        let detail = admin(
            &client,
            &base,
            &format!("/requests/{request_id}"),
            "GET",
            None,
        )
        .await;
        let record = &detail["request"];
        assert_eq!(record["state"], "completed", "{detail}");
        assert_eq!(record["account_id"], account_id);
        assert_eq!(record["upstream_attempts"], 1);
        assert_eq!(record["upstream_transport"], "websocket");
        ws_request_ids.push(request_id);
        assert_eq!(record["usage"]["input_tokens"], 12);
        assert_eq!(record["usage"]["output_tokens"], 3);
        assert!(!detail.to_string().contains("PRIVATE_WS_INPUT"));
        if turn == 1 {
            binding = record["binding_id"].clone();
        } else {
            assert_eq!(record["binding_id"], binding);
        }
    }
    assert_eq!(
        mock.connections.load(Ordering::SeqCst),
        1,
        "All HTTP requests must use one upstream WS"
    );
    let captures = mock.captures.lock().await;
    assert_eq!(captures.len(), 3);
    assert_eq!(captures[2]["previous_response_id"], previous_response_id);
    assert_eq!(captures[2]["input"].as_array().unwrap().len(), 1);
    assert_ne!(
        captures[0]["client_metadata"]["x-codex-turn-metadata"],
        captures[1]["client_metadata"]["x-codex-turn-metadata"]
    );
    for (payload, label) in captures.iter().zip(["ASCII", "中文", "😀"]) {
        let metadata: Value = serde_json::from_str(
            payload["client_metadata"]["x-codex-turn-metadata"]
                .as_str()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(metadata["tool_namespaces_info"], label);
        assert_eq!(metadata["turn_id"], payload["client_metadata"]["turn_id"]);
    }
    drop(captures);
    // Stateless HTTP still uses WS, but cannot retain a conversation socket.
    for _ in 0..2 {
        let response = client
            .post(format!("{base}/v1/responses"))
            .bearer_auth(&key)
            .json(&json!({"model":"ws-test","input":"stateless"}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        assert_eq!(
            response.json::<Value>().await.unwrap()["status"],
            "completed"
        );
    }
    assert_eq!(mock.connections.load(Ordering::SeqCst), 3);
    assert_eq!(mock.http_requests.load(Ordering::SeqCst), 0);

    // Two native clients with the SAME Key and identity must not share parents.
    let mut native = connect(&base, &key).await;
    let full = json!({"type":"response.create","model":"ws-test","input":[],"generate":false,"client_metadata":{"turn_id":"","x-codex-turn-metadata":json!({"turn_id":"","request_kind":"prewarm"}).to_string()}});
    let warmup = exchange(&mut native, full.clone()).await;
    assert_eq!(warmup["type"], "response.completed", "{warmup}");
    assert_eq!(warmup["response"]["output"], json!([]));
    assert_eq!(warmup["response"]["usage"]["output_tokens"], 0);
    let delta = json!({"type":"response.create","model":"ws-test","previous_response_id":warmup["response"]["id"],"input":[{"role":"user","content":"delta"}],"client_metadata":{"turn_id":"native-turn-1","parent_turn_id":"parent-1","x-codex-turn-metadata":json!({"turn_id":"native-turn-1","parent_turn_id":"parent-1","request_kind":"turn"}).to_string()}});
    let continued = exchange(&mut native, delta.clone()).await;
    assert_eq!(continued["type"], "response.completed", "{continued}");
    let mut next = delta.clone();
    next["previous_response_id"] = continued["response"]["id"].clone();
    next["client_metadata"] = json!({"turn_id":"native-turn-2","parent_turn_id":"parent-2","x-codex-turn-metadata":json!({"turn_id":"native-turn-2","parent_turn_id":"parent-2","request_kind":"turn"}).to_string()});
    let next_response = exchange(&mut native, next).await;
    assert_eq!(
        next_response["type"], "response.completed",
        "{next_response}"
    );
    {
        let captures = mock.captures.lock().await;
        assert_eq!(captures[5]["generate"], false);
        let turns: Vec<Value> = captures[5..8]
            .iter()
            .map(|r| {
                serde_json::from_str(
                    r["client_metadata"]["x-codex-turn-metadata"]
                        .as_str()
                        .unwrap(),
                )
                .unwrap()
            })
            .collect();
        assert_eq!(turns[0]["turn_id"], "");
        assert_eq!(turns[0]["request_kind"], "prewarm");
        for index in 1..=2 {
            assert_eq!(
                turns[index]["turn_id"],
                captures[index + 5]["client_metadata"]["turn_id"]
            );
            assert_eq!(
                turns[index]["parent_turn_id"],
                captures[index + 5]["client_metadata"]["parent_turn_id"]
            );
            assert_eq!(turns[index]["request_kind"], "turn");
            assert_ne!(turns[index]["turn_id"], "");
        }
        assert_ne!(turns[1]["turn_id"], turns[2]["turn_id"]);
        assert_ne!(turns[1]["parent_turn_id"], turns[2]["parent_turn_id"]);
        assert_eq!(
            captures[7]["previous_response_id"],
            continued["response"]["id"]
        );
        assert_eq!(captures[6]["input"], delta["input"]);
        assert_eq!(
            captures[6]["previous_response_id"],
            warmup["response"]["id"]
        );
    }
    assert_eq!(mock.connections.load(Ordering::SeqCst), 4);
    let mut second = connect(&base, &key).await;
    let stolen = exchange(&mut second, delta.clone()).await;
    assert_eq!(
        stolen["response"]["error"]["code"], "previous_response_not_found",
        "{stolen}"
    );
    assert_eq!(mock.captures.lock().await.len(), 8);
    assert_eq!(
        exchange(&mut second, full.clone()).await["type"],
        "response.completed"
    );
    assert_eq!(mock.connections.load(Ordering::SeqCst), 5);

    // Once an upstream socket is gone, reject deltas before issuing any create.
    mock.close_after.store(1, Ordering::SeqCst);
    let closed = exchange(&mut native, full.clone()).await;
    let calls = mock.captures.lock().await.len();
    let missing = exchange(&mut native, json!({"type":"response.create","model":"ws-test","previous_response_id":closed["response"]["id"],"input":[]})).await;
    assert_eq!(
        missing["response"]["error"]["code"], "previous_response_not_found",
        "{missing}"
    );
    assert_eq!(mock.captures.lock().await.len(), calls);
    assert_eq!(
        exchange(&mut native, full.clone()).await["type"],
        "response.completed"
    );
    assert_eq!(mock.connections.load(Ordering::SeqCst), 6);

    // Closing a downstream stream releases its gateway execution and capacity.
    let mut cancelled = connect(&base, &key).await;
    mock.stall_next.store(1, Ordering::SeqCst);
    cancelled
        .send(ClientMessage::Text(full.to_string().into()))
        .await
        .unwrap();
    let created = tokio::time::timeout(std::time::Duration::from_secs(5), cancelled.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(matches!(created, ClientMessage::Text(text) if text.contains("response.created")));
    cancelled.close(None).await.unwrap();
    drop(cancelled);
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while gateway.scheduler.stats().inflight != 0 {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("disconnect must release execution capacity");

    fallback_contract::verify(&client, &base, &key, &gateway, &mock).await;
    let http_requests_before = mock.http_requests.load(Ordering::SeqCst);

    // Account policy is persisted and controls both ingress transports.
    let detail = admin(
        &client,
        &base,
        &format!("/accounts/{account_id}"),
        "GET",
        None,
    )
    .await;
    let current = &detail["account"];
    let updated = admin(
        &client,
        &base,
        &format!("/accounts/{account_id}"),
        "PUT",
        Some(json!({
            "version":current["version"],"name":current["name"],"max_inflight":2,
            "models":current["models"],"responses_websocket_enabled":false
        })),
    )
    .await;
    assert_eq!(updated["responses_websocket_enabled"], false);
    let unavailable = exchange(&mut native, full.clone()).await;
    assert_eq!(unavailable["type"], "error");
    assert_eq!(
        unavailable["error"]["code"], "no_available_account",
        "{unavailable}"
    );
    let ordinary = client
        .post(format!("{base}/responses"))
        .bearer_auth(&key)
        .json(&json!({"model":"ws-test","input":"HTTP account"}))
        .send()
        .await
        .unwrap();
    let http_request_id = ordinary.headers()["x-request-id"]
        .to_str()
        .unwrap()
        .to_owned();
    assert_eq!(
        ordinary.json::<Value>().await.unwrap()["status"],
        "completed"
    );
    assert_eq!(
        mock.http_requests.load(Ordering::SeqCst),
        http_requests_before + 1
    );

    // List and detail retain the dispatched transport after account policy changes,
    // including successes compacted to summary-only records at Info level.
    for (id, transport) in ws_request_ids
        .iter()
        .map(|id| (id, "websocket"))
        .chain(std::iter::once((&http_request_id, "http")))
    {
        let detail = admin(&client, &base, &format!("/requests/{id}"), "GET", None).await;
        assert_eq!(detail["request"]["upstream_transport"], transport);
        let list = admin(&client, &base, &format!("/requests?id={id}"), "GET", None).await;
        assert_eq!(list["total"], 1);
        assert_eq!(list["items"][0]["upstream_transport"], transport);
        assert_eq!(list["items"][0]["upstream_attempts"], 1);
    }

    // Every native message re-authenticates; an established socket grants no bypass.
    let keys = admin(&client, &base, "/keys", "GET", None).await;
    let key_id = keys["items"][0]["id"].as_str().unwrap();
    admin(
        &client,
        &base,
        &format!("/keys/{key_id}/enabled"),
        "PUT",
        Some(json!({"enabled":false})),
    )
    .await;
    let revoked = exchange(&mut second, full).await;
    assert_eq!(revoked["status"], 401, "{revoked}");
    let _ = native.close(None).await;
    let _ = second.close(None).await;
    gateway.shutdown.cancel();
    gateway.tasks.close();
    gateway.tasks.wait().await;
    task.abort();
    upstream_task.abort();
    drop(gateway);
    drop(store);
    sqlx::query(&format!("DROP SCHEMA {schema} CASCADE"))
        .execute(&pool)
        .await
        .unwrap();
}

async fn http_upstream(
    State(mock): State<Mock>,
    headers: http::HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    mock.http_requests.fetch_add(1, Ordering::SeqCst);
    mock.http_captures.lock().await.push((headers, body));
    if let Some(response) = fallback_contract::http_failure(&mock) {
        return response;
    }
    let event = json!({"type":"response.completed","response":{"id":"resp_http","status":"completed","output":[],"usage":{"input_tokens":1,"output_tokens":1}}});
    (
        [("content-type", "text/event-stream")],
        format!("data: {event}\n\n"),
    )
        .into_response()
}

type ClientSocket = WebSocketStream<tokio::net::TcpStream>;
async fn connect(base: &str, key: &str) -> ClientSocket {
    let url = url::Url::parse(base).unwrap();
    let tcp = tokio::net::TcpStream::connect((url.host_str().unwrap(), url.port().unwrap()))
        .await
        .unwrap();
    let mut request = format!("{}/v1/responses", base.replacen("http", "ws", 1))
        .into_client_request()
        .unwrap();
    request
        .headers_mut()
        .insert("authorization", format!("Bearer {key}").parse().unwrap());
    request
        .headers_mut()
        .insert("session-id", "native-same-session".parse().unwrap());
    request
        .headers_mut()
        .insert("thread-id", "native-same-thread".parse().unwrap());
    request.headers_mut().insert("x-codex-turn-metadata", r#"{"turn_id":"handshake-turn","parent_turn_id":"handshake-parent","request_kind":"websocket_connection"}"#.parse().unwrap());
    tokio_tungstenite::client_async(request, tcp)
        .await
        .unwrap()
        .0
}
async fn exchange(socket: &mut ClientSocket, request: Value) -> Value {
    socket
        .send(ClientMessage::Text(request.to_string().into()))
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            match socket.next().await.unwrap().unwrap() {
                ClientMessage::Text(text) => {
                    let event: Value = serde_json::from_str(&text).unwrap();
                    if matches!(
                        event["type"].as_str(),
                        Some("response.completed" | "response.failed" | "error")
                    ) {
                        return event;
                    }
                }
                ClientMessage::Ping(_) => socket.flush().await.unwrap(),
                other => panic!("unexpected message {other:?}"),
            }
        }
    })
    .await
    .expect("native response timed out")
}
