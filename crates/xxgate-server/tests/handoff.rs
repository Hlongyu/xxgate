//! Real processes, real PostgreSQL and HTTP: the successor cannot dispatch
//! before the old stream is complete; queued traffic is forwarded exactly once.
use axum::{
    Json, Router,
    body::Body,
    extract::{State, WebSocketUpgrade, ws::Message as ServerMessage},
    response::IntoResponse,
    routing::post,
};
use base64::{Engine, engine::general_purpose::STANDARD};
use chrono::Utc;
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{
    process::{Child, Command},
    sync::Semaphore,
};
use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest};
use uuid::Uuid;
use xxgate_core::{
    access::{CredentialCipher, GatewayKey, secret_hash},
    accounts::{Account, ClientProfile, Credentials},
    application::ports::{AccessStore, AccountStore, SettingsStore},
    providers::ModelSpec,
    types::ModelRef,
};
use xxgate_postgres::PgStore;

const TOKEN: &str = "handoff-test-cluster-token-0123456789";
const SECRET: &str = "sk-handoff-test";

#[derive(Clone)]
struct Upstream {
    calls: Arc<AtomicUsize>,
    release: Arc<Semaphore>,
}
async fn generate(State(mock): State<Upstream>, Json(body): Json<Value>) -> impl IntoResponse {
    mock.calls.fetch_add(1, Ordering::SeqCst);
    let hold = body["instructions"] == "hold";
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<bytes::Bytes, std::io::Error>>(2);
    tokio::spawn(async move {
        let _ = tx.send(Ok("data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_test\",\"status\":\"in_progress\"}}\n\n".into())).await;
        if hold {
            mock.release.acquire().await.unwrap().forget();
        }
        let event = json!({"type":"response.completed","response":{"id":"resp_test","model":"handoff-model","status":"completed","output":[],"usage":{"input_tokens":10,"output_tokens":2}}});
        let _ = tx.send(Ok(format!("data: {event}\n\n").into())).await;
    });
    (
        [("content-type", "text/event-stream")],
        Body::from_stream(tokio_stream::wrappers::ReceiverStream::new(rx)),
    )
}
async fn generate_ws(State(mock): State<Upstream>, ws: WebSocketUpgrade) -> impl IntoResponse {
    ws.on_upgrade(move |mut socket| async move {
        while let Some(Ok(ServerMessage::Text(text)))=socket.recv().await {
            mock.calls.fetch_add(1, Ordering::SeqCst);
            let request:Value=serde_json::from_str(&text).unwrap();
            let created=json!({"type":"response.created","response":{"id":"resp_ws_test","status":"in_progress"}});
            if socket.send(ServerMessage::Text(created.to_string().into())).await.is_err() { break; }
            if request["instructions"]=="hold" { mock.release.acquire().await.unwrap().forget(); }
            let event=json!({"type":"response.completed","response":{"id":"resp_ws_test","model":"handoff-model","status":"completed","output":[],"usage":{"input_tokens":10,"output_tokens":2}}});
            if socket.send(ServerMessage::Text(event.to_string().into())).await.is_err() {break;}
        }
    })
}
fn address() -> String {
    let socket = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    format!("http://{}", socket.local_addr().unwrap())
}
fn start(database: &str, id: &str, role: &str, url: &str, standby: bool) -> Child {
    Command::new(env!("CARGO_BIN_EXE_xxgate-server"))
        .env("DATABASE_URL", database)
        .env("XXGATE_MASTER_KEY", STANDARD.encode([7; 32]))
        .env("XXGATE_ADMIN_PASSWORD", "handoff-test-password")
        .env("XXGATE_NODE_ID", id)
        .env("XXGATE_ROLE", role)
        .env("XXGATE_NODE_URL", url)
        .env("XXGATE_BIND", url.trim_start_matches("http://"))
        .env("XXGATE_CLUSTER_TOKEN", TOKEN)
        .env("XXGATE_ALLOW_HTTP_UPSTREAM", "1")
        .env("XXGATE_SECURE_COOKIES", "0")
        .env("XXGATE_DB_POOL_SIZE", "4")
        .env(
            "XXGATE_WAIT_FOR_NODE_LOCK_SECS",
            if standby { "60" } else { "0" },
        )
        .env("NO_PROXY", "127.0.0.1,localhost")
        .env("HTTPS_PROXY", "http://127.0.0.1:9")
        .env("RUST_LOG", "error")
        .kill_on_drop(true)
        .spawn()
        .unwrap()
}
async fn status(client: &reqwest::Client, url: &str) -> Option<Value> {
    client
        .get(format!("{url}/internal/deployment"))
        .header("x-xxgate-cluster-token", TOKEN)
        .timeout(Duration::from_secs(1))
        .send()
        .await
        .ok()?
        .json()
        .await
        .ok()
}
async fn wait_phase(client: &reqwest::Client, url: &str, phase: &str) -> Value {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if let Some(v) = status(client, url).await
                && v["phase"] == phase
            {
                return v;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{url} did not reach {phase}"))
}
async fn request(client: &reqwest::Client, url: &str, hold: bool) -> reqwest::Response {
    client.post(format!("{url}/v1/responses")).bearer_auth(SECRET)
        .json(&json!({"model":"handoff-model","input":"test","stream":true,"instructions":if hold {"hold"} else {"go"},"client_metadata":{"session_id":"stable-session","thread_id":"thread"}}))
        .send().await.unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires disposable PostgreSQL; scripts/test.py worker_handoff"]
async fn worker_handoff_keeps_stream_and_waits_for_ownership() {
    let database = std::env::var("TEST_DATABASE_URL").unwrap();
    let admin = sqlx::PgPool::connect(&database).await.unwrap();
    let name = format!("handoff_{}", Uuid::new_v4().simple());
    sqlx::query(&format!("CREATE DATABASE {name}"))
        .execute(&admin)
        .await
        .unwrap();
    let mut db = url::Url::parse(&database).unwrap();
    db.set_path(&name);
    let store = PgStore::connect(db.as_str())
        .await
        .unwrap()
        .for_node("default")
        .unwrap();
    let mock = Upstream {
        calls: Default::default(),
        release: Arc::new(Semaphore::new(0)),
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_url = format!("http://{}", listener.local_addr().unwrap());
    let app = Router::new()
        .route("/responses", post(generate).get(generate_ws))
        .with_state(mock.clone());
    let upstream = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let account:Account=serde_json::from_value(json!({"id":Uuid::new_v4(),"name":"test","provider":"openai","access_kind":"codex_oauth","enabled":true,"max_inflight":1,"upstream_account_id":"test","upstream_base_url":upstream_url,"models":["handoff-model"],"responses_websocket_enabled":true,"profile":ClientProfile::default(),"version":0,"credential_version":0,"created_at":Utc::now(),"updated_at":Utc::now()})).unwrap();
    let credentials: Credentials = serde_json::from_value(
        json!({"access_token":"test","refresh_token":"","id_token":"","expires_at":null}),
    )
    .unwrap();
    store
        .put_account(
            &account,
            Some(
                &CredentialCipher::new(&[7; 32])
                    .encrypt(account.id, &credentials)
                    .unwrap(),
            ),
            None,
        )
        .await
        .unwrap();
    store
        .put_model(&ModelSpec {
            id: "handoff-model".into(),
            upstream: ModelRef::codex("handoff-model"),
            enabled: true,
            capabilities: Default::default(),
            version: 0,
        })
        .await
        .unwrap();
    let key = GatewayKey {
        id: Uuid::new_v4(),
        group_id: xxgate_core::groups::DEFAULT_GROUP_ID,
        name: "test".into(),
        prefix: "sk-test".into(),
        enabled: true,
        created_at: Utc::now(),
        last_used_at: None,
    };
    store.put_key(&key, &secret_hash(SECRET)).await.unwrap();
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let old_url = address();
    let new_url = address();
    let entry_url = address();
    let mut old = start(db.as_str(), "default", "worker", &old_url, false);
    let old_status = wait_phase(&client, &old_url, "serving").await;
    let mut entry = start(db.as_str(), "entry", "router", &entry_url, false);
    wait_phase(&client, &entry_url, "serving").await;
    assert_eq!(
        client
            .get(format!("{old_url}/internal/deployment"))
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    let mut next = start(db.as_str(), "default", "worker", &new_url, true);
    let mut ws_request = format!("{}/v1/responses", entry_url.replace("http://", "ws://"))
        .into_client_request()
        .unwrap();
    ws_request
        .headers_mut()
        .insert("authorization", format!("Bearer {SECRET}").parse().unwrap());
    let (mut websocket, _) = tokio_tungstenite::connect_async(ws_request).await.unwrap();
    let standby = wait_phase(&client, &new_url, "standby").await;
    assert_eq!(
        client
            .get(format!("{new_url}/readyz"))
            .send()
            .await
            .unwrap()
            .status(),
        503
    );
    let mut competing = start(db.as_str(), "default", "worker", &address(), true);
    assert!(
        !tokio::time::timeout(Duration::from_secs(5), competing.wait())
            .await
            .unwrap()
            .unwrap()
            .success()
    );
    assert_eq!(status(&client, &old_url).await.unwrap()["phase"], "serving");
    let wrong=client.post(format!("{old_url}/internal/deployment/handoff")).header("x-xxgate-cluster-token",TOKEN)
        .json(&json!({"expected_boot_id":Uuid::new_v4(),"successor_url":new_url,"expected_successor_boot_id":standby["boot_id"]})).send().await.unwrap();
    assert_eq!(wrong.status(), 409);
    let first = request(&client, &entry_url, true).await;
    assert!(first.status().is_success());
    let mut stream = first.bytes_stream();
    loop {
        let chunk = stream.next().await.unwrap().unwrap();
        if String::from_utf8_lossy(&chunk).contains("response.created") {
            break;
        }
    }
    let probing = Arc::new(AtomicBool::new(true));
    let probe = tokio::spawn({
        let client = client.clone();
        let base = entry_url.clone();
        let probing = probing.clone();
        async move {
            while probing.load(Ordering::SeqCst) {
                assert_eq!(
                    client
                        .get(format!("{base}/readyz"))
                        .send()
                        .await
                        .unwrap()
                        .status(),
                    200
                );
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }
    });
    // A timeout must resume the old release and leave the long stream intact.
    for timeout in [1, 10] {
        let result=client.post(format!("{old_url}/internal/deployment/handoff"))
            .header("x-xxgate-cluster-token",TOKEN)
            .json(&json!({"expected_boot_id":old_status["boot_id"],"successor_url":new_url,"expected_successor_boot_id":standby["boot_id"],"drain_timeout_secs":timeout}))
            .send().await.unwrap();
        assert!(
            result.status().is_success(),
            "{}",
            result.text().await.unwrap()
        );
        wait_phase(&client, &old_url, "draining").await;
        if timeout == 1 {
            wait_phase(&client, &old_url, "serving").await;
        }
    }
    // Newly admitted request waits at the old bridge. It must not execute twice.
    let pending = tokio::spawn({
        let client = client.clone();
        let url = entry_url.clone();
        async move {
            let r = request(&client, &url, false).await;
            assert!(r.status().is_success());
            r.text().await.unwrap()
        }
    });
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(mock.calls.load(Ordering::SeqCst), 1);
    assert_eq!(status(&client, &new_url).await.unwrap()["phase"], "standby");
    assert!(!pending.is_finished());
    mock.release.add_permits(1);
    let mut tail = Vec::new();
    while let Some(chunk) = stream.next().await {
        tail.extend(chunk.unwrap());
    }
    assert!(
        String::from_utf8(tail)
            .unwrap()
            .contains("response.completed")
    );
    let result = tokio::time::timeout(Duration::from_secs(30), pending)
        .await
        .unwrap()
        .unwrap();
    assert!(result.contains("response.completed"));
    wait_phase(&client, &old_url, "forwarding").await;
    let successor = wait_phase(&client, &new_url, "serving").await;
    assert_ne!(successor["boot_id"], old_status["boot_id"]);
    assert_eq!(mock.calls.load(Ordering::SeqCst), 2);
    // Direct stale worker URL forwards once; unauthenticated internal calls
    // never acquire the bridge's cluster credentials.
    assert_eq!(
        client
            .post(format!("{old_url}/internal/reload"))
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    assert!(
        request(&client, &old_url, false)
            .await
            .text()
            .await
            .unwrap()
            .contains("response.completed")
    );
    assert_eq!(mock.calls.load(Ordering::SeqCst), 3);
    websocket.send(Message::Text(json!({"type":"response.create","model":"handoff-model","input":"test","client_metadata":{"session_id":"stable-session","thread_id":"thread"}}).to_string().into())).await.unwrap();
    loop {
        let event = tokio::time::timeout(Duration::from_secs(5), websocket.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        if let Message::Text(text) = event {
            let value: Value = serde_json::from_str(&text).unwrap();
            assert_ne!(value["type"], "error", "{value}");
            if value["type"] == "response.completed" {
                break;
            }
        }
    }
    websocket.close(None).await.unwrap();
    assert_eq!(mock.calls.load(Ordering::SeqCst), 4);
    // Same session still has one binding/account after the process replacement.
    let pool = sqlx::PgPool::connect(db.as_str()).await.unwrap();
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM requests WHERE finished_at IS NOT NULL AND state='completed'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(count, 4);
    probing.store(false, Ordering::SeqCst);
    probe.await.unwrap();
    for child in [&mut old, &mut next, &mut entry] {
        child.kill().await.unwrap();
        child.wait().await.unwrap();
    }
    upstream.abort();
    pool.close().await;
    sqlx::query(&format!("DROP DATABASE {name} WITH (FORCE)"))
        .execute(&admin)
        .await
        .unwrap();
    admin.close().await;
}
