//! Two isolated account pools, one router, one disposable PostgreSQL schema.
use axum::{
    Json, Router,
    extract::{State, WebSocketUpgrade, ws::Message},
    http::HeaderMap,
    response::{IntoResponse, Response},
    routing::post,
};
use chrono::Utc;
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use sqlx::Connection;
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::{Mutex, Semaphore};
use tokio_tungstenite::tungstenite::{Message as ClientMessage, client::IntoClientRequest};
use uuid::Uuid;
use xxgate_codex::{ingress::ResponsesIngress, provider::CodexProvider};
use xxgate_core::{
    access::{CredentialCipher, GatewayKey, hash_password, secret_hash},
    accounts::{Account, ClientProfile, Credentials},
    application::{
        gateway::Gateway,
        ports::{
            AccessStore, AccountStore, IdentityStore, ReportStore, RequestStore, SettingsStore,
        },
    },
    identity::SessionKey,
    memory::{MemoryDomain, MemoryProbe, MemorySample},
    providers::ModelSpec,
    reports::RequestFilter,
    types::ModelRef,
};
use xxgate_postgres::PgStore;
use xxgate_server::{
    cluster::Cluster,
    http::{AppState, router},
};
use xxgate_transport::HttpTransport;

struct Healthy;
impl MemoryProbe for Healthy {
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
#[derive(Clone)]
struct Mock {
    calls: Arc<AtomicUsize>,
    hold: Arc<AtomicBool>,
    release: Arc<Semaphore>,
    accounts: Arc<Mutex<Vec<String>>>,
    ws_requests: Arc<Mutex<Vec<Value>>>,
    fail_next_ws: Arc<AtomicBool>,
    large_body_verified: Arc<AtomicBool>,
}
async fn upstream(
    State(mock): State<Mock>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    if body["instructions"] == "large-body-preservation" {
        assert_eq!(body["input"][0]["content"], "large-input-".repeat(65536));
        assert_eq!(
            body["tools"][0]["parameters"]["description"],
            "large-schema-".repeat(16384)
        );
        mock.large_body_verified.store(true, Ordering::SeqCst);
    }
    mock.calls.fetch_add(1, Ordering::SeqCst);
    mock.accounts.lock().await.push(
        headers
            .get("chatgpt-account-id")
            .unwrap()
            .to_str()
            .unwrap()
            .into(),
    );
    if mock.hold.load(Ordering::SeqCst) {
        mock.release.acquire().await.unwrap().forget();
    }
    let id = format!("resp_{}", Uuid::new_v4().simple());
    let body=[json!({"type":"response.created","response":{"id":id,"model":"cluster-model","status":"in_progress"}}),json!({"type":"response.completed","response":{"id":id,"object":"response","model":"cluster-model","status":"completed","output":[],"usage":{"input_tokens":10,"output_tokens":2}}})]
        .iter().map(|e|format!("data: {e}\n\n")).collect::<String>();
    ([("content-type", "text/event-stream")], body).into_response()
}
async fn upstream_ws(
    State(mock): State<Mock>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    ws.on_upgrade(move |mut socket|async move {
        while let Some(Ok(Message::Text(text)))=socket.recv().await {
            let request:Value=serde_json::from_str(&text).unwrap();assert_eq!(request["type"],"response.create");
            mock.ws_requests.lock().await.push(request);
            mock.calls.fetch_add(1,Ordering::SeqCst);
            mock.accounts.lock().await.push(headers["chatgpt-account-id"].to_str().unwrap().into());
            if mock.fail_next_ws.swap(false,Ordering::SeqCst) { return; }
            let id=format!("resp_{}",Uuid::new_v4().simple());
            for event in [json!({"type":"response.created","response":{"id":id,"status":"in_progress"}}),json!({"type":"response.completed","response":{"id":id,"status":"completed","model":"cluster-model","output":[],"usage":{"input_tokens":10,"output_tokens":2}}})] {
                if socket.send(Message::Text(event.to_string().into())).await.is_err(){return;}
            }
        }
    }).into_response()
}
async fn serve(app: Router) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    (
        url,
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() }),
    )
}
async fn start(
    store: Arc<PgStore>,
    entry: bool,
) -> (String, AppState, tokio::task::JoinHandle<()>) {
    let gateway = Gateway::new_with_memory_probe(
        store.clone(),
        CredentialCipher::new(&[7; 32]),
        Arc::new(ResponsesIngress),
        Arc::new(CodexProvider),
        |s| Arc::new(HttpTransport::new(s, true)),
        Arc::new(Healthy),
    )
    .await
    .unwrap();
    let mut state = AppState::new(gateway, true, false);
    let boot = Uuid::new_v4();
    state.cluster = Some(
        Cluster::new(
            store.clone(),
            entry,
            "test-cluster-token-01234567890123456789".into(),
            boot,
        )
        .unwrap(),
    );
    let (url, task) = serve(router(state.clone())).await;
    store
        .register_node(if entry { "router" } else { "worker" }, &url, boot)
        .await
        .unwrap();
    state
        .cluster
        .as_ref()
        .unwrap()
        .reload(&state)
        .await
        .unwrap();
    state
        .cluster
        .as_ref()
        .unwrap()
        .publish(&state)
        .await
        .unwrap();
    (url, state, task)
}
async fn create_account(store: &PgStore, name: &str, url: &str) -> Account {
    let a:Account=serde_json::from_value(json!({"id":Uuid::new_v4(),"name":name,"provider":"openai","access_kind":"codex_oauth","enabled":true,"max_inflight":1,"upstream_account_id":name,"upstream_base_url":url,"models":["cluster-model"],"profile":ClientProfile::default(),"version":0,"credential_version":0,"created_at":Utc::now(),"updated_at":Utc::now()})).unwrap();
    let credentials:Credentials=serde_json::from_value(json!({"access_token":"PRIVATE_TEST_TOKEN","refresh_token":"","id_token":"","expires_at":null})).unwrap();
    store
        .put_account(
            &a,
            Some(
                &CredentialCipher::new(&[7; 32])
                    .encrypt(a.id, &credentials)
                    .unwrap(),
            ),
            None,
        )
        .await
        .unwrap()
}
async fn admin(
    client: &reqwest::Client,
    base: &str,
    path: &str,
    method: &str,
    body: Option<Value>,
) -> Value {
    let mut r = client
        .request(method.parse().unwrap(), format!("{base}/api/admin{path}"))
        .header("x-xxgate-csrf", "1");
    if let Some(body) = body {
        r = r.json(&body);
    }
    let r = r.send().await.unwrap();
    let status = r.status();
    let body: Value = r.json().await.unwrap();
    assert!(status.is_success(), "{path}: {status} {body}");
    body
}
async fn response(
    client: &reqwest::Client,
    base: &str,
    secret: &str,
    session: Option<&str>,
    stream: bool,
) -> reqwest::Response {
    let mut body = json!({"model":"cluster-model","input":"PRIVATE_PROMPT","stream":stream});
    // Body-only identities must route identically to header identities.
    if let Some(session) = session {
        body["client_metadata"] = json!({"session_id":session,"thread_id":session});
    }
    client
        .post(format!("{base}/v1/responses"))
        .bearer_auth(secret)
        .json(&body)
        .send()
        .await
        .unwrap()
}
async fn wait_for(mut predicate: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while !predicate() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires disposable PostgreSQL; run scripts/test.py cluster_gateway_contract"]
async fn cluster_gateway_contract() {
    let database = std::env::var("TEST_DATABASE_URL").unwrap();
    let pool = sqlx::PgPool::connect(&database).await.unwrap();
    let schema = format!("cluster_{}", Uuid::new_v4().simple());
    sqlx::query(&format!("CREATE DATABASE {schema}"))
        .execute(&pool)
        .await
        .unwrap();
    let mut url = url::Url::parse(&database).unwrap();
    url.set_path(&schema);
    let root = PgStore::connect(url.as_str()).await.unwrap();
    let a = Arc::new(root.for_node("worker-a").unwrap());
    let b = Arc::new(root.for_node("worker-b").unwrap());
    let entry = Arc::new(root.for_node("entry").unwrap());
    let lock_a = a.single_instance_lock().await.unwrap();
    let lock_b = b.single_instance_lock().await.unwrap();
    assert!(a.single_instance_lock().await.is_err());
    root.initialize_admin(&hash_password("cluster-test-password").unwrap())
        .await
        .unwrap();
    root.put_model(&ModelSpec {
        id: "cluster-model".into(),
        upstream: ModelRef::codex("cluster-model"),
        enabled: true,
        capabilities: Default::default(),
        version: 0,
    })
    .await
    .unwrap();
    let secret = "sk-cluster-test-key";
    let key = GatewayKey {
        id: Uuid::new_v4(),
        group_id: xxgate_core::groups::DEFAULT_GROUP_ID,
        name: "shared".into(),
        prefix: "sk-cluster".into(),
        enabled: true,
        created_at: Utc::now(),
        last_used_at: None,
    };
    root.put_key(&key, &secret_hash(secret)).await.unwrap();
    let mock = Mock {
        calls: Default::default(),
        hold: Default::default(),
        release: Arc::new(Semaphore::new(0)),
        accounts: Default::default(),
        ws_requests: Default::default(),
        fail_next_ws: Default::default(),
        large_body_verified: Default::default(),
    };
    let (upstream_url, upstream_task) = serve(
        Router::new()
            .route("/responses", post(upstream).get(upstream_ws))
            .with_state(mock.clone()),
    )
    .await;
    let aa = create_account(&a, "upstream-a", &upstream_url).await;
    let bb = create_account(&b, "upstream-b", &upstream_url).await;
    assert_eq!(a.accounts().await.unwrap().len(), 1);
    assert_eq!(b.accounts().await.unwrap().len(), 1);
    assert!(entry.accounts().await.unwrap().is_empty());
    assert_eq!(
        b.set_enabled(aa.id, false, None, aa.version, "admin")
            .await
            .unwrap_err()
            .code,
        "account_node_mismatch"
    );
    let (_, sa, ta) = start(a.clone(), false).await;
    let (b_url, sb, tb) = start(b.clone(), false).await;
    let (base, se, te) = start(entry.clone(), true).await;
    let client = reqwest::Client::builder()
        .no_proxy()
        .cookie_store(true)
        .timeout(Duration::from_secs(15))
        .build()
        .unwrap();
    admin(
        &client,
        &base,
        "/login",
        "POST",
        Some(json!({"password":"cluster-test-password"})),
    )
    .await;
    let accounts = admin(&client, &base, "/accounts", "GET", None).await;
    assert_eq!(accounts["items"].as_array().unwrap().len(), 2);
    assert!(accounts.to_string().contains("worker-a"));
    assert!(accounts.to_string().contains("worker-b"));
    assert!(!accounts.to_string().contains("PRIVATE_TEST_TOKEN"));
    let models: Value = client
        .get(format!("{base}/v1/models"))
        .bearer_auth(secret)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        models["data"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["id"] == "cluster-model")
    );
    let r = response(&client, &base, secret, Some("sticky"), false).await;
    assert_eq!(r.status(), 200);
    let node = r.headers()["x-xxgate-node"].to_str().unwrap().to_owned();
    let request_id = r.headers()["x-request-id"].to_str().unwrap().to_owned();
    assert_eq!(r.json::<Value>().await.unwrap()["status"], "completed");
    for _ in 0..3 {
        let r = response(&client, &base, secret, Some("sticky"), true).await;
        assert_eq!(r.headers()["x-xxgate-node"], node);
        assert!(r.text().await.unwrap().contains("response.completed"));
    }
    let detail = admin(
        &client,
        &base,
        &format!("/requests/{request_id}"),
        "GET",
        None,
    )
    .await;
    assert_eq!(detail["request"]["node_id"], node);
    let large = client.post(format!("{base}/v1/responses")).bearer_auth(secret)
        .header("session-id", "large-payload")
        .json(&json!({"model":"cluster-model","input":[{"role":"user","content":"large-input-".repeat(65536)}],
            "instructions":"large-body-preservation","tools":[{"type":"function","name":"test","parameters":{"description":"large-schema-".repeat(16384)}}]}))
        .send().await.unwrap();
    assert_eq!(large.status(), 200);
    assert_eq!(large.json::<Value>().await.unwrap()["status"], "completed");
    assert!(mock.large_body_verified.load(Ordering::SeqCst));
    // One placement winner for simultaneous first arrivals across routers/workers.
    let session = SessionKey {
        key_id: key.id,
        client_session_id: "placement-race".into(),
    };
    let (x, y) = tokio::join!(
        a.claim_session(&session, "worker-a"),
        b.claim_session(&session, "worker-b")
    );
    assert_eq!(x.unwrap(), y.unwrap());
    // Fresh sessions occupy both independent capacity pools at the same time.
    let parallel_calls = mock.calls.load(Ordering::SeqCst);
    mock.hold.store(true, Ordering::SeqCst);
    let r1 = response(&client, &base, secret, Some("parallel-a"), true).await;
    let r2 = response(&client, &base, secret, Some("parallel-b"), true).await;
    assert_ne!(r1.headers()["x-xxgate-node"], r2.headers()["x-xxgate-node"]);
    wait_for(|| {
        sa.gateway.scheduler.stats().inflight == 1 && sb.gateway.scheduler.stats().inflight == 1
    })
    .await;
    wait_for(|| mock.calls.load(Ordering::SeqCst) == parallel_calls + 2).await;
    // Restart reconciliation for the entry must not finalize either worker's request.
    assert_eq!(entry.reconcile_interrupted().await.unwrap(), 0);
    let active = root
        .requests(&RequestFilter {
            state: Some("inflight".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(active["total"], 2);
    mock.hold.store(false, Ordering::SeqCst);
    mock.release.add_permits(2);
    assert!(r1.text().await.unwrap().contains("response.completed"));
    assert!(r2.text().await.unwrap().contains("response.completed"));
    wait_for(|| {
        sa.gateway.scheduler.stats().inflight == 0 && sb.gateway.scheduler.stats().inflight == 0
    })
    .await;
    let history = admin(&client, &base, "/requests?node_id=worker-b", "GET", None).await;
    assert!(history["total"].as_i64().unwrap() > 0);
    assert!(
        history["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|r| r["node_id"] == "worker-b")
    );
    // Disconnecting an SSE client releases the worker's lease through the router.
    let before_cancel = mock.calls.load(Ordering::SeqCst);
    mock.hold.store(true, Ordering::SeqCst);
    let cancelled = response(&client, &base, secret, Some("cancel-through-entry"), true).await;
    wait_for(|| mock.calls.load(Ordering::SeqCst) == before_cancel + 1).await;
    drop(cancelled);
    wait_for(|| {
        sa.gateway.scheduler.stats().inflight == 0 && sb.gateway.scheduler.stats().inflight == 0
    })
    .await;
    mock.hold.store(false, Ordering::SeqCst);
    mock.release.add_permits(1);
    // Account administration is sent to its owner, including when another node is selected.
    let r = client
        .put(format!("{base}/api/admin/accounts/{}/enabled", bb.id))
        .header("x-xxgate-csrf", "1")
        .header("x-xxgate-node", "worker-a")
        .json(&json!({"enabled":false}))
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success(), "{}", r.text().await.unwrap());
    assert!(!sb.gateway.scheduler.account(bb.id).unwrap().enabled);
    assert!(sa.gateway.scheduler.account(aa.id).unwrap().enabled);
    admin(
        &client,
        &base,
        &format!("/accounts/{}/enabled", bb.id),
        "PUT",
        Some(json!({"enabled":true})),
    )
    .await;
    // Shared configuration pushes reach both nodes before the admin response.
    let mut settings = admin(&client, &base, "/settings", "GET", None).await;
    settings["session_max_inflight"] = json!(3);
    admin(&client, &base, "/settings", "PUT", Some(settings)).await;
    assert_eq!(sa.gateway.settings.current().session_max_inflight, 3);
    assert_eq!(sb.gateway.settings.current().session_max_inflight, 3);
    // Public callers cannot invoke cluster controls or forge a WS connection identity.
    assert_eq!(
        client
            .post(format!("{b_url}/internal/reload"))
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    // Creation, callbacks and cancellation retain the selected OAuth worker.
    let flow: Value = client
        .post(format!("{base}/api/admin/oauth/browser"))
        .header("x-xxgate-csrf", "1")
        .header("x-xxgate-node", "worker-b")
        .json(&json!({"name":"new OAuth account"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let flow_id = flow["id"].as_str().expect("OAuth flow ID");
    assert_eq!(
        root.operation_node(Uuid::parse_str(flow_id).unwrap())
            .await
            .unwrap(),
        "worker-b"
    );
    let status = client
        .get(format!("{base}/api/admin/oauth/browser/{flow_id}"))
        .header("x-xxgate-node", "worker-a")
        .send()
        .await
        .unwrap();
    assert_eq!(status.status(), 200);
    admin(
        &client,
        &base,
        &format!("/oauth/browser/{flow_id}"),
        "DELETE",
        None,
    )
    .await;
    // Stateless traffic is spread without inventing persistent session mappings.
    let scoped_pool = sqlx::PgPool::connect(url.as_str()).await.unwrap();
    let sessions_before: i64 = sqlx::query_scalar("SELECT count(*) FROM session_nodes")
        .fetch_one(&scoped_pool)
        .await
        .unwrap();
    let mut stateless_nodes = std::collections::HashSet::new();
    for _ in 0..4 {
        let r = response(&client, &base, secret, None, false).await;
        assert_eq!(r.status(), 200);
        stateless_nodes.insert(r.headers()["x-xxgate-node"].to_str().unwrap().to_owned());
        r.bytes().await.unwrap();
    }
    assert_eq!(stateless_nodes.len(), 2);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM session_nodes")
            .fetch_one(&scoped_pool)
            .await
            .unwrap(),
        sessions_before
    );
    // A group with no accounts cannot use another group's workers.
    use xxgate_core::application::ports::GroupStore;
    let group = root.create_group("empty group").await.unwrap();
    let isolated = GatewayKey {
        id: Uuid::new_v4(),
        group_id: group.id,
        ..key.clone()
    };
    root.put_key(&isolated, &secret_hash("sk-isolated-group"))
        .await
        .unwrap();
    let before = mock.calls.load(Ordering::SeqCst);
    let r = response(&client, &base, "sk-isolated-group", Some("isolated"), false).await;
    assert_eq!(r.status(), 503);
    assert_eq!(mock.calls.load(Ordering::SeqCst), before);
    // Original WS ingress traverses the router and executes on the assigned worker.
    let current = sb.gateway.scheduler.account(bb.id).unwrap();
    admin(&client,&base,&format!("/accounts/{}",bb.id),"PUT",Some(json!({"name":current.name,"version":current.version,"max_inflight":1,"models":["cluster-model"],"responses_websocket_enabled":true}))).await;
    root.claim_session(
        &SessionKey {
            key_id: key.id,
            client_session_id: "native-ws".into(),
        },
        "worker-b",
    )
    .await
    .unwrap();
    let mut request = format!("{}/v1/responses", base.replace("http://", "ws://"))
        .into_client_request()
        .unwrap();
    request
        .headers_mut()
        .insert("authorization", format!("Bearer {secret}").parse().unwrap());
    request
        .headers_mut()
        .insert("session-id", "native-ws".parse().unwrap());
    request.headers_mut().insert(
        "x-codex-turn-metadata",
        r#"{"turn_id":"handshake-turn","request_kind":"websocket_connection"}"#
            .parse()
            .unwrap(),
    );
    let (mut socket, _) = tokio_tungstenite::connect_async(request).await.unwrap();
    for turn in ["", "turn-1", "turn-2"] {
        let prewarm = turn.is_empty();
        socket.send(ClientMessage::Text(json!({"type":"response.create","model":"cluster-model","input":"PRIVATE_WS_INPUT","generate":!prewarm,"client_metadata":{"turn_id":turn,"x-codex-turn-metadata":json!({"turn_id":turn,"request_kind":if prewarm {"prewarm"} else {"turn"}}).to_string()}}).to_string().into())).await.unwrap();
        loop {
            let message = tokio::time::timeout(Duration::from_secs(5), socket.next())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            if let ClientMessage::Text(text) = message {
                let event: Value = serde_json::from_str(&text).unwrap();
                assert_ne!(event["type"], "error", "{event}");
                assert_ne!(event["type"], "response.failed", "turn {turn:?}: {event}");
                if event["type"] == "response.completed" {
                    break;
                }
            }
        }
    }
    socket.close(None).await.unwrap();
    {
        let requests = mock.ws_requests.lock().await;
        assert_eq!(requests.len(), 3);
        let turns: Vec<Value> = requests
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
        assert_eq!(requests[0]["generate"], false);
        assert_eq!(turns[0]["turn_id"], "");
        assert_eq!(turns[0]["request_kind"], "prewarm");
        for index in 1..=2 {
            assert_eq!(
                turns[index]["turn_id"],
                requests[index]["client_metadata"]["turn_id"]
            );
            assert_eq!(turns[index]["request_kind"], "turn");
            assert_ne!(turns[index]["turn_id"], "");
        }
        assert_ne!(turns[1]["turn_id"], turns[2]["turn_id"]);
    }
    assert_eq!(mock.accounts.lock().await.last().unwrap(), "upstream-b");
    wait_for(|| sb.gateway.scheduler.stats().inflight == 0).await;
    // The worker falls back for HTTP ingress; entry routing and account stay fixed.
    root.claim_session(
        &SessionKey {
            key_id: key.id,
            client_session_id: "http-fallback-cluster".into(),
        },
        "worker-b",
    )
    .await
    .unwrap();
    let calls = mock.calls.load(Ordering::SeqCst);
    mock.fail_next_ws.store(true, Ordering::SeqCst);
    let fallback = response(&client, &base, secret, Some("http-fallback-cluster"), true).await;
    let request_id = fallback.headers()["x-request-id"]
        .to_str()
        .unwrap()
        .to_owned();
    assert!(
        fallback
            .text()
            .await
            .unwrap()
            .contains("response.completed")
    );
    assert_eq!(mock.calls.load(Ordering::SeqCst), calls + 2);
    let detail = admin(
        &client,
        &base,
        &format!("/requests/{request_id}"),
        "GET",
        None,
    )
    .await;
    assert_eq!(detail["request"]["node_id"], "worker-b");
    assert_eq!(detail["request"]["account_id"], bb.id.to_string());
    assert_eq!(detail["request"]["upstream_attempts"], 2);
    assert_eq!(detail["request"]["upstream_transport"], "http");
    assert!(
        detail["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["kind"] == "websocket_http_fallback")
    );
    // Node totals add up to the unfiltered cluster report, including rejections.
    let all = admin(&client, &base, "/dashboard", "GET", None).await;
    let mut total = 0;
    for id in ["worker-a", "worker-b", "entry"] {
        total += admin(
            &client,
            &base,
            &format!("/dashboard?node_id={id}"),
            "GET",
            None,
        )
        .await["summary"]["requests"]
            .as_i64()
            .unwrap();
    }
    assert_eq!(all["summary"]["requests"].as_i64().unwrap(), total);
    let before = mock.calls.load(Ordering::SeqCst);
    // An offline owner fails explicitly; its session is never replayed on the other pool.
    let owner = if node == "worker-a" { &a } else { &b };
    sqlx::query("UPDATE cluster_nodes SET heartbeat_at=now()-interval '1 minute' WHERE id=$1")
        .bind(owner.node_id())
        .execute(&scoped_pool)
        .await
        .unwrap();
    let r = response(&client, &base, secret, Some("sticky"), false).await;
    assert_eq!(r.status(), 503);
    assert_eq!(
        r.json::<Value>().await.unwrap()["error"]["code"],
        "node_unavailable"
    );
    assert_eq!(mock.calls.load(Ordering::SeqCst), before);
    // Recovery only finalizes the restarting worker, and repeated recovery is idempotent.
    let mut pending: xxgate_core::audit::RequestRecord =
        serde_json::from_value(detail["request"].clone()).unwrap();
    pending.id = Uuid::new_v4();
    pending.account_id = Some(aa.id);
    pending.finished_at = None;
    pending.state = "inflight".into();
    pending.usage = Default::default();
    pending.valuation = None;
    a.begin_request(&pending).await.unwrap();
    pending.id = Uuid::new_v4();
    pending.account_id = Some(bb.id);
    b.begin_request(&pending).await.unwrap();
    assert_eq!(a.reconcile_interrupted().await.unwrap(), 1);
    assert_eq!(
        root.request_detail(pending.id).await.unwrap()["request"]["state"],
        "inflight"
    );
    assert_eq!(a.reconcile_interrupted().await.unwrap(), 0);
    assert_eq!(b.reconcile_interrupted().await.unwrap(), 1);
    // Bindings loaded by a worker never include the other worker's accounts.
    assert!(
        a.active_bindings()
            .await
            .unwrap()
            .iter()
            .all(|binding| binding.account_id == aa.id)
    );
    assert!(
        b.active_bindings()
            .await
            .unwrap()
            .iter()
            .all(|binding| binding.account_id == bb.id)
    );
    for state in [&sa, &sb, &se] {
        state.gateway.shutdown.cancel();
        state.gateway.tasks.close();
        state.gateway.tasks.wait().await;
    }
    for task in [ta, tb, te, upstream_task] {
        task.abort();
        let _ = task.await;
    }
    lock_a.close().await.unwrap();
    lock_b.close().await.unwrap();
    scoped_pool.close().await;
    sqlx::query(&format!("DROP DATABASE {schema} WITH (FORCE)"))
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires disposable PostgreSQL"]
async fn cluster_fragmented_wire_contract() {
    use axum::body::Body;
    use bytes::Bytes;
    let database = std::env::var("TEST_DATABASE_URL").unwrap();
    let pool = sqlx::PgPool::connect(&database).await.unwrap();
    let schema = format!("wire_cluster_{}", Uuid::new_v4().simple());
    sqlx::query(&format!("CREATE DATABASE {schema}"))
        .execute(&pool)
        .await
        .unwrap();
    let mut url = url::Url::parse(&database).unwrap();
    url.set_path(&schema);
    let root = PgStore::connect(url.as_str()).await.unwrap();
    let worker = root.for_node("wire-worker").unwrap();
    let mut account = create_account(&worker, "wire-account", "https://example.com").await;
    account.responses_websocket_enabled = true;
    worker
        .put_account(&account, None, Some(account.version))
        .await
        .unwrap();
    root.put_model(&ModelSpec {
        id: "cluster-model".into(),
        upstream: ModelRef::codex("cluster-model"),
        enabled: true,
        capabilities: Default::default(),
        version: 0,
    })
    .await
    .unwrap();
    let key = GatewayKey {
        id: Uuid::new_v4(),
        group_id: xxgate_core::groups::DEFAULT_GROUP_ID,
        name: "wire".into(),
        prefix: "sk-wire".into(),
        enabled: true,
        created_at: Utc::now(),
        last_used_at: None,
    };
    root.put_key(&key, &secret_hash("sk-wire-test"))
        .await
        .unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    let (worker_url, worker_task) = serve(Router::new().route(
        "/v1/responses",
        post(move || {
            let calls = counter.clone();
            async move {
                let mode = calls.fetch_add(1, Ordering::SeqCst);
                let mut wire = Vec::from(b": heartbeat\r\n\r\n".as_slice());
                for event in [
                    json!({"type":"response.output_text.delta","delta":"你好"}),
                    json!({"type":"response.completed","response":{"id":"resp_wire"}}),
                ] {
                    wire.extend_from_slice(&xxgate_codex::wire::frame(&event).unwrap());
                }
                if mode == 2 {
                    wire.truncate(wire.len() - 2);
                }
                let width = if mode == 1 { 1 } else { wire.len() };
                let chunks = wire
                    .chunks(width)
                    .map(Bytes::copy_from_slice)
                    .map(Ok::<_, std::io::Error>)
                    .collect::<Vec<_>>();
                (
                    [("content-type", "text/event-stream")],
                    Body::from_stream(futures::stream::iter(chunks)),
                )
            }
        }),
    ))
    .await;
    let boot = Uuid::new_v4();
    worker
        .register_node("worker", &worker_url, boot)
        .await
        .unwrap();
    worker
        .heartbeat(
            boot,
            true,
            json!({"capacity":1,"queue":{"inflight":0,"queued":0}}),
        )
        .await
        .unwrap();
    let (base, state, task) = start(Arc::new(root.for_node("wire-entry").unwrap()), true).await;
    for mode in 0..3 {
        let mut request = format!("{}/v1/responses", base.replace("http://", "ws://"))
            .into_client_request()
            .unwrap();
        request
            .headers_mut()
            .insert("authorization", "Bearer sk-wire-test".parse().unwrap());
        let (mut socket, _) = tokio_tungstenite::connect_async(request).await.unwrap();
        socket
            .send(ClientMessage::Text(
                json!({"type":"response.create","model":"cluster-model","input":"test"})
                    .to_string()
                    .into(),
            ))
            .await
            .unwrap();
        let mut kinds = vec![];
        loop {
            let message = tokio::time::timeout(Duration::from_secs(5), socket.next())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            if let ClientMessage::Text(text) = message {
                let event: Value = serde_json::from_str(&text).unwrap();
                let kind = event["type"].as_str().unwrap();
                kinds.push(kind.to_owned());
                if kind == "response.output_text.delta" {
                    assert_eq!(event["delta"], "你好");
                }
                if kind == "response.completed" {
                    assert!(mode < 2);
                    break;
                }
                if kind == "error" {
                    assert_eq!(mode, 2);
                    assert_eq!(event["error"]["code"], "worker_stream_interrupted");
                    break;
                }
            }
        }
        assert_eq!(kinds.len(), 2);
        let _ = socket.close(None).await;
    }
    assert_eq!(calls.load(Ordering::SeqCst), 3);
    state.gateway.shutdown.cancel();
    state.gateway.tasks.close();
    state.gateway.tasks.wait().await;
    task.abort();
    worker_task.abort();
    let _ = task.await;
    let _ = worker_task.await;
    sqlx::query(&format!("DROP DATABASE {schema} WITH (FORCE)"))
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;
}
