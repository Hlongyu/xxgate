use super::*;
use std::sync::atomic::AtomicUsize;
use tokio::task::{JoinHandle, JoinSet};
use tokio_tungstenite::{
    accept_hdr_async,
    tungstenite::handshake::server::{Request, Response},
};
use uuid::Uuid;
use xxgate_core::identity::{SessionKey, ThreadKey};
use xxgate_core::protocol::UpstreamTransport;

#[derive(Default)]
struct Observed {
    handshakes: AtomicUsize,
    payloads: Mutex<Vec<Value>>,
    mode: AtomicUsize,
}

fn mock_output() -> Value {
    json!({"type":"message","id":"msg_test","role":"assistant","status":"completed","content":[{"type":"output_text","text":"answer","annotations":[]}]})
}

struct Server {
    url: String,
    observed: Arc<Observed>,
    task: JoinHandle<()>,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Server {
    #[expect(
        clippy::result_large_err,
        reason = "The tungstenite handshake callback requires this error type"
    )]
    async fn new() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/responses", listener.local_addr().unwrap());
        let observed = Arc::new(Observed::default());
        let state = observed.clone();
        let task = tokio::spawn(async move {
            let mut tasks = JoinSet::new();
            loop {
                tokio::select! {
                    accepted = listener.accept() => {
                        let (tcp, _) = accepted.unwrap();
                        let state = state.clone();
                        tasks.spawn(async move {
                            let mut socket = accept_hdr_async(tcp, |request: &Request, mut response: Response| {
                                assert_eq!(request.uri().path(), "/responses");
                                assert_eq!(request.headers()["authorization"], "Bearer test");
                                assert_eq!(request.headers()["openai-beta"], "responses_websockets=2026-02-06");
                                state.handshakes.fetch_add(1, Ordering::SeqCst);
                                response.headers_mut().insert("x-request-id", HeaderValue::from_static("handshake-only"));
                                Ok(response)
                            }).await.unwrap();
                            while let Some(message) = socket.next().await {
                                let Ok(message) = message else { break; };
                                match message {
                                    Message::Text(text) => {
                                        state.payloads.lock().unwrap().push(serde_json::from_str(&text).unwrap());
                                        let mode = state.mode.load(Ordering::SeqCst);
                                        if mode == 1 { break; } // Ambiguous close after create.
                                        if mode == 2 {
                                            socket.send(Message::Text(json!({"type":"error","status":429,"error":{"code":"rate_limit_exceeded","message":"busy"}}).to_string().into())).await.unwrap();
                                            continue;
                                        }
                                        if mode == 7 {
                                            socket.send(Message::Text(json!({"type":"error","status":400,"error":{"code":"previous_response_not_found","message":"Missing parent"}}).to_string().into())).await.unwrap();
                                            continue;
                                        }
                                        if mode == 4 {
                                            socket.send(Message::Text("not JSON".into())).await.unwrap();
                                            continue;
                                        }
                                        if mode == 5 {
                                            socket.send(Message::Text(json!({"type":"response.output_text.delta","delta":"x".repeat(2048)}).to_string().into())).await.unwrap();
                                            continue;
                                        }
                                        if socket.send(Message::Ping(Bytes::from_static(b"ping"))).await.is_err() { break; }
                                        if socket.send(Message::Text("{\n\"type\":\"response.created\",\"response\":{\"id\":\"resp_test\"}}".into())).await.is_err() { break; }
                                        if mode == 3 { continue; } // Inflight cancellation/drop.
                                        let done = json!({"type":"response.completed","response":{"id":"resp_test","status":"completed","output":[mock_output()],"usage":{"input_tokens":5,"output_tokens":2}}});
                                        if socket.send(Message::Text(done.to_string().into())).await.is_err() { break; }
                                        if mode == 6 { break; } // Close an otherwise completed idle socket.
                                    },
                                    Message::Ping(_) => { if socket.flush().await.is_err() { break; } },
                                    Message::Close(_) => break,
                                    _ => {},
                                }
                            }
                        });
                    },
                    done = tasks.join_next(), if !tasks.is_empty() => { done.unwrap().unwrap(); },
                }
            }
        });
        Self {
            url,
            observed,
            task,
        }
    }

    fn request(&self) -> PreparedRequest {
        let mut headers = HeaderMap::new();
        headers.insert("authorization", HeaderValue::from_static("Bearer test"));
        headers.insert(
            "x-codex-turn-metadata",
            HeaderValue::from_static("{\"turn_id\":\"one\"}"),
        );
        PreparedRequest { method: http::Method::POST, url: self.url.clone(), headers,
            body: Bytes::from(json!({"model":"test","input":[{"role":"user","content":"first"}],"stream":true,"store":false}).to_string()),
            account_id: Some(Uuid::nil()), profile_version: 1, tls_backend: "rustls".into() }
    }

    fn calls(&self) -> usize {
        self.observed.payloads.lock().unwrap().len()
    }
    fn handshakes(&self) -> usize {
        self.observed.handshakes.load(Ordering::SeqCst)
    }
}

fn key() -> ResponseConnectionKey {
    ResponseConnectionKey {
        websocket_session: None,
        reusable: true,
        thread: ThreadKey {
            session: SessionKey {
                key_id: Uuid::new_v4(),
                client_session_id: "session".into(),
            },
            client_thread_id: "thread".into(),
        },
        binding_id: Uuid::new_v4(),
    }
}

async fn drain(mut response: UpstreamResponse) -> Result<Vec<Value>> {
    assert_eq!(response.status, 200);
    let mut events = Vec::new();
    while let Some(bytes) = response.bytes.next().await {
        let bytes = bytes?;
        let text = std::str::from_utf8(&bytes).unwrap();
        assert!(text.starts_with("data: ") && text.ends_with("\n\n"));
        assert_eq!(
            text.lines().count(),
            2,
            "Pretty JSON must be compacted for SSE"
        );
        events.push(serde_json::from_str(text[6..].trim()).unwrap());
    }
    Ok(events)
}

async fn run(
    transport: &HttpTransport,
    request: PreparedRequest,
    key: ResponseConnectionKey,
) -> Vec<Value> {
    let response = transport
        .send_response(request, Some(key), CancellationToken::new())
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), drain(response))
        .await
        .unwrap()
        .unwrap()
}

#[tokio::test]
async fn reuses_socket_with_incremental_input_and_per_turn_metadata() {
    let server = Server::new().await;
    let transport = HttpTransport::new(LiveSettings::new(Default::default()), true);
    let key = key();
    let first = run(&transport, server.request(), key.clone()).await;
    assert_eq!(
        first.last().unwrap()["response"]["usage"]["output_tokens"],
        2
    );
    let mut request = server.request();
    request.headers.insert(
        "x-codex-turn-metadata",
        HeaderValue::from_static("{\"turn_id\":\"two\"}"),
    );
    request.body = Bytes::from(json!({"model":"test","stream":true,"store":false,"input":[{"role":"user","content":"first"},mock_output(),{"role":"user","content":"next"}]}).to_string());
    let response = transport
        .send_response(request, Some(key), CancellationToken::new())
        .await
        .unwrap();
    assert!(
        response.headers.get("x-request-id").is_none(),
        "Never reuse handshake request IDs or quota headers"
    );
    drain(response).await.unwrap();
    assert_eq!(server.handshakes(), 1);
    let payloads = server.observed.payloads.lock().unwrap();
    assert_eq!(payloads.len(), 2);
    assert_eq!(
        payloads[1]["input"],
        json!([{"role":"user","content":"next"}])
    );
    assert_eq!(payloads[1]["previous_response_id"], "resp_test");
    assert!(payloads[0].get("previous_response_id").is_none());
    for payload in payloads.iter() {
        assert_eq!(payload["type"], "response.create");
        assert_eq!(payload["store"], false);
        assert!(payload.get("stream").is_none());
    }
    assert_eq!(
        payloads[1]["client_metadata"]["x-codex-turn-metadata"],
        "{\"turn_id\":\"two\"}"
    );
}

#[tokio::test]
async fn preserves_utf8_turn_metadata_on_new_and_reused_sockets() {
    let server = Server::new().await;
    let transport = HttpTransport::new(LiveSettings::new(Default::default()), true);
    let key = key();
    let mut expected = Vec::new();
    for (turn, label) in [("one", "中文"), ("two", "😀")] {
        let metadata = json!({"turn_id":turn,"tool_namespaces_info":label}).to_string();
        let mut request = server.request();
        request.headers.insert(
            "x-codex-turn-metadata",
            HeaderValue::from_str(&metadata).unwrap(),
        );
        run(&transport, request, key.clone()).await;
        expected.push(metadata);
    }
    assert_eq!(server.handshakes(), 1);
    let payloads = server.observed.payloads.lock().unwrap();
    assert_eq!(payloads.len(), expected.len());
    for (payload, metadata) in payloads.iter().zip(expected) {
        assert_eq!(
            payload["client_metadata"]["x-codex-turn-metadata"],
            metadata
        );
    }
}

#[tokio::test]
async fn rejects_non_utf8_turn_metadata_before_connecting() {
    let server = Server::new().await;
    let transport = HttpTransport::new(LiveSettings::new(Default::default()), true);
    let mut request = server.request();
    request.headers.insert(
        "x-codex-turn-metadata",
        HeaderValue::from_bytes(b"{\"tool_namespaces_info\":\"\xff\"}").unwrap(),
    );
    let error = transport
        .send_response(request, Some(key()), CancellationToken::new())
        .await
        .err()
        .expect("non-UTF-8 metadata must be rejected");
    assert_eq!(error.status, 400);
    assert_eq!(error.code, "invalid_request");
    assert_eq!(error.message, "Invalid turn metadata");
    assert_eq!(server.handshakes(), 0);
    assert_eq!(server.calls(), 0);
}

#[tokio::test]
async fn missing_parent_is_not_retried_and_next_request_sends_full_history() {
    let server = Server::new().await;
    let transport = HttpTransport::new(LiveSettings::new(Default::default()), true);
    let key = key();
    run(&transport, server.request(), key.clone()).await;
    let mut request = server.request();
    request.body = Bytes::from(json!({"model":"test","stream":true,"store":false,"input":[{"role":"user","content":"first"},mock_output(),{"role":"user","content":"next"}]}).to_string());
    server.observed.mode.store(7, Ordering::SeqCst);
    let error = run(&transport, request.clone(), key.clone()).await;
    assert_eq!(error[0]["error"]["code"], "previous_response_not_found");
    assert_eq!(
        server.calls(),
        2,
        "An explicit error still must not cause hidden retries"
    );
    server.observed.mode.store(0, Ordering::SeqCst);
    run(&transport, request, key).await;
    let payloads = server.observed.payloads.lock().unwrap();
    assert_eq!(payloads[1]["previous_response_id"], "resp_test");
    assert!(payloads[2].get("previous_response_id").is_none());
    assert_eq!(payloads[2]["input"].as_array().unwrap().len(), 3);
    assert_eq!(server.handshakes(), 2);
}

#[tokio::test]
async fn disabling_incremental_keeps_socket_but_sends_full_history() {
    let server = Server::new().await;
    let settings = LiveSettings::new(Default::default());
    let transport = HttpTransport::new(settings.clone(), true);
    let key = key();
    run(&transport, server.request(), key.clone()).await;
    let mut cfg = settings.current();
    cfg.responses_websocket_incremental_enabled = false;
    settings.publish(cfg);
    let mut request = server.request();
    request.body = Bytes::from(json!({"model":"test","stream":true,"store":false,"input":[{"role":"user","content":"first"},mock_output(),{"role":"user","content":"next"}]}).to_string());
    run(&transport, request, key).await;
    let payloads = server.observed.payloads.lock().unwrap();
    assert!(payloads[1].get("previous_response_id").is_none());
    assert_eq!(payloads[1]["input"].as_array().unwrap().len(), 3);
    assert_eq!(server.handshakes(), 1);
}

#[tokio::test]
async fn isolates_threads_keys_bindings_profiles_and_credentials() {
    let server = Server::new().await;
    let transport = HttpTransport::new(LiveSettings::new(Default::default()), true);
    let first = key();
    run(&transport, server.request(), first.clone()).await;
    let mut thread = first.clone();
    thread.thread.client_thread_id = "other".into();
    run(&transport, server.request(), thread).await;
    let mut other_key = first.clone();
    other_key.thread.session.key_id = Uuid::new_v4();
    run(&transport, server.request(), other_key).await;
    let mut migrated = first.clone();
    migrated.binding_id = Uuid::new_v4();
    run(&transport, server.request(), migrated.clone()).await;
    let mut updated = server.request();
    updated.profile_version = 2;
    run(&transport, updated.clone(), migrated.clone()).await;
    updated.headers.insert(
        "chatgpt-account-id",
        HeaderValue::from_static("new-auth-scope"),
    );
    run(&transport, updated, migrated).await;
    assert_eq!(server.handshakes(), 6);
}

#[tokio::test]
async fn cancellation_and_dropped_streams_discard_connections() {
    let server = Server::new().await;
    server.observed.mode.store(3, Ordering::SeqCst);
    let transport = HttpTransport::new(LiveSettings::new(Default::default()), true);
    let key = key();
    let cancel = CancellationToken::new();
    let mut response = transport
        .send_response(server.request(), Some(key.clone()), cancel.clone())
        .await
        .unwrap();
    response.bytes.next().await.unwrap().unwrap();
    cancel.cancel();
    assert_eq!(
        response.bytes.next().await.unwrap().unwrap_err().code,
        "client_cancelled"
    );
    drop(response);
    let mut response = transport
        .send_response(
            server.request(),
            Some(key.clone()),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    response.bytes.next().await.unwrap().unwrap();
    drop(response);
    server.observed.mode.store(0, Ordering::SeqCst);
    run(&transport, server.request(), key).await;
    assert_eq!(server.handshakes(), 3);
    assert_eq!(server.calls(), 3);
}

#[tokio::test]
async fn closes_errors_and_never_replays_an_ambiguous_request() {
    let server = Server::new().await;
    let transport = HttpTransport::new(LiveSettings::new(Default::default()), true);
    let key = key();
    for mode in [1, 2, 4] {
        server.observed.mode.store(mode, Ordering::SeqCst);
        let response = transport
            .send_response(
                server.request(),
                Some(key.clone()),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let result = drain(response).await;
        if mode == 2 {
            assert_eq!(result.unwrap()[0]["error"]["code"], "rate_limit_exceeded");
        } else {
            assert!(result.is_err());
        }
    }
    server.observed.mode.store(0, Ordering::SeqCst);
    run(&transport, server.request(), key).await;
    assert_eq!(server.handshakes(), 4);
    assert_eq!(server.calls(), 4);
}

#[tokio::test]
async fn replaces_closed_idle_socket_before_sending_next_request() {
    let server = Server::new().await;
    server.observed.mode.store(6, Ordering::SeqCst);
    let transport = HttpTransport::new(LiveSettings::new(Default::default()), true);
    let key = key();
    run(&transport, server.request(), key.clone()).await;
    server.observed.mode.store(0, Ordering::SeqCst);
    run(&transport, server.request(), key).await;
    assert_eq!(server.handshakes(), 2);
    assert_eq!(server.calls(), 2);
}

#[tokio::test]
async fn enforces_event_limit_and_discards_oversized_socket() {
    let server = Server::new().await;
    server.observed.mode.store(5, Ordering::SeqCst);
    let settings = LiveSettings::new(Default::default());
    let mut next = settings.current();
    next.sse_event_limit_bytes = 1024;
    settings.publish(next);
    let transport = HttpTransport::new(settings, true);
    let response = transport
        .send_response(server.request(), Some(key()), CancellationToken::new())
        .await
        .unwrap();
    assert!(drain(response).await.is_err());
    assert!(transport.responses_ws.0.idle.lock().unwrap().is_empty());
    assert_eq!(server.calls(), 1);
}

#[tokio::test]
async fn idle_ttl_and_connection_age_expire_without_reuse() {
    let server = Server::new().await;
    let transport = HttpTransport::new(LiveSettings::new(Default::default()), true);
    let key = key();
    run(&transport, server.request(), key.clone()).await;
    transport
        .responses_ws
        .0
        .idle
        .lock()
        .unwrap()
        .get_mut(&key)
        .unwrap()
        .idle_since = Instant::now() - IDLE_TTL;
    run(&transport, server.request(), key.clone()).await;
    transport
        .responses_ws
        .0
        .idle
        .lock()
        .unwrap()
        .get_mut(&key)
        .unwrap()
        .created = Instant::now() - MAX_AGE;
    run(&transport, server.request(), key).await;
    assert_eq!(server.handshakes(), 3);
}

#[tokio::test]
async fn ws_routing_never_falls_back_to_http() {
    use axum::{Router, routing::get};
    for status in [405, 401, 429, 503] {
        let gets = Arc::new(AtomicUsize::new(0));
        let posts = Arc::new(AtomicUsize::new(0));
        let g = gets.clone();
        let p = posts.clone();
        let app = Router::new().route(
            "/responses",
            get(move || {
                let g = g.clone();
                async move {
                    g.fetch_add(1, Ordering::SeqCst);
                    (StatusCode::from_u16(status).unwrap(), "rejected")
                }
            })
            .post(move || {
                let p = p.clone();
                async move {
                    p.fetch_add(1, Ordering::SeqCst);
                    "HTTP"
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/responses", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let transport = HttpTransport::new(LiveSettings::new(Default::default()), true);
        let request = PreparedRequest {
            method: http::Method::POST,
            url,
            headers: HeaderMap::new(),
            body: Bytes::from_static(b"{\"input\":\"hello\"}"),
            account_id: Some(Uuid::nil()),
            profile_version: 1,
            tls_backend: "rustls".into(),
        };
        let response = transport
            .send_response(request.clone(), Some(key()), CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(response.status, status);
        assert_eq!(gets.load(Ordering::SeqCst), 1);
        assert_eq!(posts.load(Ordering::SeqCst), 0);
        let mut cfg = transport.settings.current();
        cfg.responses_websocket_enabled = false;
        transport.settings.publish(cfg);
        transport
            .send_response(request.clone(), Some(key()), CancellationToken::new())
            .await
            .unwrap();
        transport
            .send_response(request, None, CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(
            gets.load(Ordering::SeqCst),
            2,
            "legacy global switch cannot override the account route"
        );
        assert_eq!(posts.load(Ordering::SeqCst), 1);
        task.abort();
    }
}

#[tokio::test]
async fn full_pool_rejects_without_sending_or_falling_back() {
    let server = Server::new().await;
    let transport = HttpTransport::new(LiveSettings::new(Default::default()), true);
    let _all = transport
        .responses_ws
        .0
        .slots
        .clone()
        .acquire_many_owned(MAX_CONNECTIONS as u32)
        .await
        .unwrap();
    let error = transport
        .send_response(server.request(), Some(key()), CancellationToken::new())
        .await
        .err()
        .unwrap();
    assert_eq!(error.code, "websocket_capacity_exhausted");
    assert_eq!(server.handshakes(), 0);
    assert_eq!(server.calls(), 0);
}

#[tokio::test]
async fn native_session_cleanup_and_stateless_requests_do_not_retain_sockets() {
    let server = Server::new().await;
    let transport = HttpTransport::new(LiveSettings::new(Default::default()), true);
    let mut native = key();
    let session = Uuid::new_v4();
    native.websocket_session = Some(session);
    run(&transport, server.request(), native.clone()).await;
    assert_eq!(transport.responses_ws.0.idle.lock().unwrap().len(), 1);
    transport.close_response_session(session);
    assert!(transport.responses_ws.0.idle.lock().unwrap().is_empty());
    let mut delta = server.request();
    delta.body = Bytes::from(json!({"input":[],"previous_response_id":"resp_test"}).to_string());
    let error = transport
        .send_response(delta, Some(native), CancellationToken::new())
        .await
        .err()
        .unwrap();
    assert_eq!(error.code, "previous_response_not_found");
    let mut stateless = key();
    stateless.reusable = false;
    run(&transport, server.request(), stateless).await;
    assert!(transport.responses_ws.0.idle.lock().unwrap().is_empty());
    assert_eq!(server.calls(), 2);
}
