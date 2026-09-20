#[path = "support/client_sources.rs"]
mod client_sources_contract;

#[path = "support/identity.rs"]
mod identity_contract;

#[path = "support/search.rs"]
mod search_contract;

#[path = "support/compaction.rs"]
mod compaction_contract;

#[path = "support/model_routing.rs"]
mod model_routing_contract;

#[path = "support/session_concurrency.rs"]
mod session_concurrency_contract;

#[path = "support/encrypted_reasoning_recovery.rs"]
mod encrypted_reasoning_recovery_contract;

#[path = "support/turn_state.rs"]
mod turn_state_contract;

use axum::{
    Json, Router,
    body::Body,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use bytes::Bytes;
use chrono::Utc;
use serde_json::{Value, json};
use std::{
    convert::Infallible,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::{Mutex, Semaphore, mpsc};
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use xxgate_codex::{ingress::ResponsesIngress, provider::CodexProvider};
use xxgate_core::protocol::{PreparedRequest, UpstreamResponse, UpstreamTransport};
use xxgate_core::{
    access::{CredentialCipher, hash_password},
    application::{
        gateway::Gateway,
        ports::{AccessStore, AccountStore, IdentityStore, ReportStore, RequestStore},
    },
    reports::{RequestFilter, UsageFilter},
};
use xxgate_postgres::PgStore;
use xxgate_server::http::{AppState, router};
use xxgate_transport::HttpTransport;

struct TestTransport {
    http: HttpTransport,
    device_calls: AtomicUsize,
    browser_calls: Arc<AtomicUsize>,
}
#[async_trait::async_trait]
impl UpstreamTransport for TestTransport {
    async fn send_once(
        &self,
        request: PreparedRequest,
        cancel: CancellationToken,
    ) -> xxgate_core::Result<UpstreamResponse> {
        if request.url == "https://auth.openai.com/api/accounts/deviceauth/usercode" {
            let first = self.device_calls.fetch_add(1, Ordering::SeqCst) == 0;
            let body = if first {
                json!({"error":{"code":"unsupported_country_region_territory","message":"PRIVATE_OAUTH_MESSAGE"}})
            } else {
                json!({"access_token":"PRIVATE_OAUTH_TOKEN"})
            };
            return Ok(UpstreamResponse {
                status: if first { 403 } else { 200 },
                headers: HeaderMap::from_iter([
                    (
                        "content-type".parse().unwrap(),
                        "application/json".parse().unwrap(),
                    ),
                    ("cf-ray".parse().unwrap(), "test-ray-HKG".parse().unwrap()),
                ]),
                bytes: Box::pin(futures::stream::once(async move {
                    Ok(Bytes::from(body.to_string()))
                })),
            });
        }
        if request.url == "https://auth.openai.com/oauth/token" {
            self.browser_calls.fetch_add(1, Ordering::SeqCst);
            let form = url::form_urlencoded::parse(&request.body)
                .collect::<std::collections::HashMap<_, _>>();
            assert_eq!(form["grant_type"], "authorization_code");
            assert_eq!(form["code"], "PRIVATE_BROWSER_CODE");
            assert_eq!(form["redirect_uri"], "http://localhost:1455/auth/callback");
            assert!(form["code_verifier"].len() >= 43);
            use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
            let claims=URL_SAFE_NO_PAD.encode(json!({"https://api.openai.com/auth":{"chatgpt_account_id":"browser-oauth-account"}}).to_string());
            let body = json!({"access_token":format!("header.{claims}.signature"),"refresh_token":"PRIVATE_BROWSER_REFRESH","expires_in":3600});
            return Ok(UpstreamResponse {
                status: 200,
                headers: HeaderMap::new(),
                bytes: Box::pin(futures::stream::once(async move {
                    Ok(Bytes::from(body.to_string()))
                })),
            });
        }
        if request
            .url
            .starts_with("https://chatgpt.com/backend-api/codex/models?")
        {
            return Ok(UpstreamResponse {
                status: 200,
                headers: HeaderMap::new(),
                bytes: Box::pin(futures::stream::once(async {
                    Ok(Bytes::from_static(
                        br#"{"models":[{"slug":"oauth-only-model","display_name":"OAuth model"}]}"#,
                    ))
                })),
            });
        }
        self.http.send_once(request, cancel).await
    }
}

#[derive(Clone)]
struct Mock {
    calls: Arc<AtomicUsize>,
    captures: Arc<Mutex<Vec<(HeaderMap, Value)>>>,
    release: Arc<Semaphore>,
    models_fail: Arc<std::sync::atomic::AtomicBool>,
    resets: Arc<Mutex<ResetMock>>,
    search: Arc<Mutex<search_contract::SearchMock>>,
    search_release: Arc<Semaphore>,
    compact: Arc<Mutex<Vec<(HeaderMap, Value)>>>,
}
struct ResetMock {
    credits: Vec<Value>,
    calls: Vec<Value>,
    redeemed: std::collections::HashSet<String>,
    fail_list: bool,
    lose_response: bool,
    nothing_next: bool,
}
impl ResetMock {
    fn new() -> Self {
        let now = Utc::now();
        let credit = |id: &str, days: Option<i64>, status: &str, kind: &str| json!({"id":id,"reset_type":kind,"status":status,"granted_at":now-chrono::Duration::days(2),"expires_at":days.map(|d|now+chrono::Duration::days(d)),"title":format!("重置 {id}"),"description":"测试重置记录"});
        Self {
            credits: vec![
                credit("late", Some(5), "available", "codex_rate_limits"),
                credit("expired", Some(-1), "available", "codex_rate_limits"),
                credit("no-expiry", None, "available", "codex_rate_limits"),
                credit("other-type", Some(0), "available", "other"),
                credit("early", Some(1), "available", "codex_rate_limits"),
                credit("used", Some(3), "redeemed", "codex_rate_limits"),
                credit("busy", Some(2), "redeeming", "codex_rate_limits"),
            ],
            calls: vec![],
            redeemed: Default::default(),
            fail_list: false,
            lose_response: false,
            nothing_next: false,
        }
    }
}
async fn reset_list(State(mock): State<Mock>, headers: HeaderMap) -> Response {
    assert_eq!(headers["chatgpt-account-id"], "mock-account-A");
    assert_eq!(headers["authorization"], "Bearer PRIVATE_ACCESS_TOKEN_A");
    assert!(
        headers["user-agent"]
            .to_str()
            .unwrap()
            .contains("codex_cli_rs/0.153.4")
    );
    let resets = mock.resets.lock().await;
    if resets.fail_list {
        return Json(json!({"available_count":0})).into_response();
    }
    let count = resets
        .credits
        .iter()
        .filter(|c| {
            c["status"] == "available"
                && c["reset_type"] == "codex_rate_limits"
                && c["id"] != "expired"
        })
        .count();
    Json(json!({"available_count":count,"credits":resets.credits})).into_response()
}
async fn reset_consume(
    State(mock): State<Mock>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    assert_eq!(headers["chatgpt-account-id"], "mock-account-A");
    assert_eq!(headers["authorization"], "Bearer PRIVATE_ACCESS_TOKEN_A");
    assert_eq!(body.as_object().unwrap().len(), 2);
    let mut resets = mock.resets.lock().await;
    resets.calls.push(body.clone());
    let key = body["redeem_request_id"].as_str().unwrap().to_owned();
    if resets.redeemed.contains(&key) {
        return Json(json!({"code":"already_redeemed","windows_reset":0})).into_response();
    }
    if std::mem::take(&mut resets.nothing_next) {
        return Json(json!({"code":"nothing_to_reset","windows_reset":0})).into_response();
    }
    let credit = resets
        .credits
        .iter_mut()
        .find(|c| c["id"] == body["credit_id"])
        .unwrap();
    assert_eq!(credit["status"], "available");
    credit["status"] = json!("redeemed");
    resets.redeemed.insert(key);
    if std::mem::take(&mut resets.lose_response) {
        return (StatusCode::OK, "{truncated response").into_response();
    }
    Json(json!({"code":"reset","windows_reset":2})).into_response()
}

async fn upstream_models(
    State(mock): State<Mock>,
    headers: HeaderMap,
    axum::extract::Query(query): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Response {
    assert_eq!(
        query.get("client_version").map(String::as_str),
        Some("0.153.4")
    );
    assert!(
        headers["authorization"]
            .to_str()
            .unwrap()
            .starts_with("Bearer PRIVATE_ACCESS_TOKEN_")
    );
    if mock.models_fail.load(Ordering::SeqCst) {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error":{"message":"PRIVATE_MODELS_ERROR"}})),
        )
            .into_response();
    }
    let only = if headers["chatgpt-account-id"] == "mock-account-A" {
        "only-a"
    } else {
        "only-b"
    };
    Json(json!({"models":[{"slug":"mock-upstream","display_name":"Shared model"},{"slug":only,"display_name":only}]})).into_response()
}
fn frame(v: Value) -> Bytes {
    Bytes::from(format!("data: {v}\n\n"))
}
async fn upstream(
    State(mock): State<Mock>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let count = mock.calls.fetch_add(1, Ordering::SeqCst) + 1;
    mock.captures.lock().await.push((headers, body.clone()));
    if body.get("max_output_tokens").is_some() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error":{"message":"Unsupported parameter: max_output_tokens"}})),
        )
            .into_response();
    }
    if let Some(input) = body["input"].as_array() {
        for item in input {
            // Model the upstream's binding between ciphertext and item ID.
            for (field, encrypted, bound_id) in [
                ("encrypted_content", "PRIVATE_LEGACY_REASONING", "rs_old"),
                ("encrypted_content", "PRIVATE_LEGACY_COMPACTION", "cmp_old"),
                ("encrypted_function_args", "PRIVATE_LEGACY_ARGS", "fc_old"),
            ] {
                if item[field] == encrypted && item["id"] != bound_id {
                    return (StatusCode::BAD_REQUEST, Json(json!({"error":{"message":"Encrypted content item_id did not match the target item id"}}))).into_response();
                }
            }
            if item["encrypted_function_args"] == "PRIVATE_LEGACY_ARGS"
                && item["call_id"] != "call_old"
            {
                return (StatusCode::BAD_REQUEST, Json(json!({"error":{"message":"Encrypted function call identity did not match"}}))).into_response();
            }
            let expected = match item["type"].as_str() {
                Some("custom_tool_call_output") => "ctco_",
                Some("function_call_output") => "fco_",
                _ => continue,
            };
            if let Some(id) = item["id"].as_str()
                && !id.starts_with(expected)
            {
                return (StatusCode::BAD_REQUEST,Json(json!({"error":{"message":format!("Invalid tool output ID; expected {expected}")}}))).into_response();
            }
        }
    }
    let behavior = body["instructions"].as_str().unwrap_or("normal").to_owned();
    if let Some(response) =
        encrypted_reasoning_recovery_contract::rejection(&mock, &body, &behavior)
    {
        return response;
    }
    if behavior == "http400" {
        return (StatusCode::BAD_REQUEST,[("x-codex-turn-state", "error-turn-state")],Json(json!({"error":{"code":"invalid_encrypted_content","message":"PRIVATE_UPSTREAM_ERROR_TEXT"}}))).into_response();
    }
    if behavior == "http429" {
        return (StatusCode::TOO_MANY_REQUESTS,Json(json!({"error":{"code":"rate_limit_exceeded","message":"PRIVATE_UPSTREAM_ERROR_TEXT"}}))).into_response();
    }
    if behavior == "http401" {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error":{"code":"invalid_token"}})),
        )
            .into_response();
    }
    let observed_behavior = behavior.clone();
    let (tx, rx) = mpsc::channel(2);
    tokio::spawn(async move {
        let response_id = format!("resp_upstream_{count}");
        let item_id = format!("msg_upstream_{count}");
        let mut created = json!({"type":"response.created","response":{"id":response_id,"object":"response","status":"in_progress"}});
        if matches!(
            behavior.as_str(),
            "model-route" | "model-created-only" | "model-cut"
        ) {
            created["response"]["model"] = json!("mock-created-model");
        }
        let created = frame(created);
        if tx.send(Ok::<_, Infallible>(created)).await.is_err() {
            return;
        }
        if behavior == "hold" || behavior == "encrypted-hold-success" {
            let permit = mock.release.acquire().await.unwrap();
            permit.forget();
        }
        if behavior == "stall" {
            let _tx = tx;
            futures::future::pending::<()>().await;
            return;
        }
        if behavior == "cut" || behavior == "model-cut" {
            return;
        }
        if behavior == "streamfail" || behavior == "encrypted-sse" {
            let code = if behavior == "encrypted-sse" {
                "invalid_encrypted_content"
            } else {
                "bad_request"
            };
            let _=tx.send(Ok(frame(json!({"type":"response.failed","response":{"id":response_id,"status":"failed","error":{"code":code,"message":"PRIVATE_UPSTREAM_ERROR_TEXT"}}})))).await;
            return;
        }
        let item = if behavior == "compact-v2" {
            json!({"type":"compaction","id":"cmp_v2","encrypted_content":"PRIVATE_V2_CIPHERTEXT","future":{"keep":true}})
        } else if behavior == "image" {
            json!({"type":"image_generation_call","id":format!("ig_{count}"),"result":"PRIVATE_IMAGE_BYTES","status":"completed","usage":{"input_tokens":30,"output_tokens":60}})
        } else {
            json!({"type":"message","id":item_id,"role":"assistant","status":"completed","content":[{"type":"output_text","text":"PRIVATE_MODEL_OUTPUT","annotations":[]}]})
        };
        let _ = tx
            .send(Ok(frame(
                json!({"type":"response.output_item.done","output_index":0,"item":item}),
            )))
            .await;
        let tier = body["service_tier"].as_str().unwrap_or("default");
        let mut completed = json!({"type":"response.completed","response":{"id":response_id,"object":"response","model":body["model"],"status":"completed","service_tier":tier,"output":[item],"usage":{"input_tokens":1000,"input_tokens_details":{"cached_tokens":400},"output_tokens":500,"output_tokens_details":{"reasoning_tokens":200},"total_tokens":1500}}});
        match behavior.as_str() {
            "model-route" => completed["response"]["model"] = json!("mock-routed-model"),
            "model-missing" | "model-created-only" => {
                completed["response"]
                    .as_object_mut()
                    .unwrap()
                    .remove("model");
            }
            _ => {}
        }
        let _ = tx.send(Ok(frame(completed))).await;
    });
    let mut response = Body::from_stream(ReceiverStream::new(rx)).into_response();
    for (k, v) in [
        ("content-type", "text/event-stream"),
        ("x-request-id", "request-from-mock"),
        ("x-codex-primary-used-percent", "20"),
        ("x-codex-primary-window-minutes", "300"),
        ("x-codex-secondary-used-percent", "40"),
        ("x-codex-secondary-window-minutes", "10080"),
    ] {
        response
            .headers_mut()
            .insert(axum::http::HeaderName::from_static(k), v.parse().unwrap());
    }
    if observed_behavior != "turn-state-none" {
        let value = if observed_behavior == "turn-state-empty" {
            String::new()
        } else {
            format!("server-turn-state-{count}")
        };
        response
            .headers_mut()
            .insert("x-codex-turn-state", value.parse().unwrap());
        if observed_behavior == "turn-state-multiple" {
            response
                .headers_mut()
                .append("x-codex-turn-state", "second-server-state".parse().unwrap());
        }
    }
    response
}
async fn serve(app: Router) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    (
        format!("http://{addr}"),
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() }),
    )
}
struct Client {
    http: reqwest::Client,
    base: String,
    key: String,
}
impl Client {
    async fn admin(&self, path: &str, method: &str, body: Option<Value>) -> Value {
        let mut request = self
            .http
            .request(
                method.parse().unwrap(),
                format!("{}/api/admin{path}", self.base),
            )
            .header("x-xxgate-csrf", "1");
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().await.unwrap();
        let status = response.status();
        let body: Value = response.json().await.unwrap();
        assert!(status.is_success(), "{path}: {status}: {body}");
        body
    }
    async fn response(
        &self,
        session: &str,
        behavior: &str,
        stream: bool,
        tier: &str,
    ) -> reqwest::Response {
        self.http.post(format!("{}/v1/responses",self.base)).bearer_auth(&self.key).header("accept-encoding","br, gzip").header("session-id",session).header("thread-id",session).header("x-codex-turn-state","DO_NOT_FORWARD").header("cookie","DO_NOT_FORWARD").json(&json!({"model":"mock-model","instructions":behavior,"input":[{"type":"message","id":"msg_client_input","role":"user","content":[{"type":"input_text","text":"PRIVATE_USER_PROMPT"}]}],"stream":stream,"service_tier":tier,"reasoning":{"effort":"medium"},"client_metadata":{"turn_id":"turn-client","parent_turn_id":"turn-parent","window_id":"window-client"}})).send().await.unwrap()
    }
    async fn enabled(&self, id: &str, value: bool) {
        self.admin(
            &format!("/accounts/{id}/enabled"),
            "PUT",
            Some(json!({"enabled":value})),
        )
        .await;
    }
    async fn legacy_response(&self, input: &Value, stream: bool) -> reqwest::Response {
        self.http
            .post(format!("{}/v1/responses", self.base))
            .bearer_auth(&self.key)
            .header("session-id", "session-legacy")
            .header("thread-id", "session-legacy")
            .json(&json!({"model":"mock-model","input":input,"stream":stream}))
            .send()
            .await
            .unwrap()
    }
    async fn record(&self, id: &str) -> Value {
        self.admin(&format!("/requests/{id}"), "GET", None).await
    }
}
fn assert_rewrite(record: &Value, field: &str, before: &str, after: &str, action: &str) {
    let event = record["events"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["kind"] == "request_rewritten")
        .unwrap();
    let entry = event["details"]["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["field"] == field)
        .unwrap();
    assert_eq!(entry["before"], before);
    assert_eq!(entry["after"], after);
    assert_eq!(entry["action"], action);
}
async fn wait_queue(gateway: &Gateway, n: usize) {
    tokio::time::timeout(Duration::from_secs(3), async {
        while gateway.scheduler.stats().queued != n {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
}
async fn wait_inflight(gateway: &Gateway, n: u32) {
    tokio::time::timeout(Duration::from_secs(3), async {
        while gateway.scheduler.stats().inflight != n {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires a disposable PostgreSQL database; run python3 scripts/test.py"]
async fn real_http_postgres_gateway_contract() {
    let database = std::env::var("TEST_DATABASE_URL").expect("TEST_DATABASE_URL is required");
    let store = Arc::new(PgStore::connect(&database).await.unwrap());
    store
        .initialize_admin(&hash_password("xxgate-test-password").unwrap())
        .await
        .unwrap();
    let mock = Mock {
        calls: Default::default(),
        captures: Default::default(),
        release: Arc::new(Semaphore::new(0)),
        models_fail: Default::default(),
        resets: Arc::new(Mutex::new(ResetMock::new())),
        search: Default::default(),
        search_release: Arc::new(Semaphore::new(0)),
        compact: Default::default(),
    };
    let (upstream_url,upstream_task)=serve(Router::new().route("/responses",post(upstream)).route("/responses/compact",post(compaction_contract::upstream_compact)).route("/alpha/search",post(search_contract::upstream_search)).route("/models",get(upstream_models)).route("/wham/rate-limit-reset-credits",get(reset_list)).route("/wham/rate-limit-reset-credits/consume",post(reset_consume)).route("/wham/usage",get(||async{Json(json!({"rate_limit":{"primary_window":{"used_percent":10,"limit_window_seconds":18000},"secondary_window":{"used_percent":25,"limit_window_seconds":604800}}}))})).with_state(mock.clone())).await;
    let browser_calls = Arc::new(AtomicUsize::new(0));
    let counted_browser = browser_calls.clone();
    let gateway = Gateway::new(
        store.clone(),
        CredentialCipher::new(&[7; 32]),
        Arc::new(ResponsesIngress),
        Arc::new(CodexProvider),
        |s| {
            Arc::new(TestTransport {
                http: HttpTransport::new(s, true),
                device_calls: AtomicUsize::new(0),
                browser_calls: counted_browser,
            })
        },
    )
    .await
    .unwrap();
    let (base, server_task) = serve(router(AppState::new(gateway.clone(), true, false))).await;
    let mut c = Client {
        http: reqwest::Client::builder()
            .cookie_store(true)
            .timeout(Duration::from_secs(10))
            .build()
            .unwrap(),
        base,
        key: String::new(),
    };
    assert_eq!(
        c.http
            .get(format!("{}/api/admin/accounts", c.base))
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    assert_eq!(
        c.http
            .post(format!("{}/api/admin/login", c.base))
            .json(&json!({"password":"xxgate-test-password"}))
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
    c.admin(
        "/login",
        "POST",
        Some(json!({"password":"xxgate-test-password"})),
    )
    .await;
    let oauth = c
        .http
        .post(format!("{}/api/admin/oauth/device", c.base))
        .header("x-xxgate-csrf", "1")
        .json(&json!({"name":"OAuth test"}))
        .send()
        .await
        .unwrap();
    assert_eq!(oauth.status(), 502);
    let oauth: Value = oauth.json().await.unwrap();
    assert_eq!(oauth["error"]["code"], "oauth_region_unsupported");
    // A malformed HTTP-200 response must release the reserved flow slot too.
    for _ in 0..17 {
        let response = c
            .http
            .post(format!("{}/api/admin/oauth/device", c.base))
            .header("x-xxgate-csrf", "1")
            .json(&json!({"name":"OAuth test"}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 502);
        assert_eq!(
            response.json::<Value>().await.unwrap()["error"]["code"],
            "invalid_oauth_response"
        );
    }
    let events = c.admin("/audit", "GET", None).await;
    assert!(!events.to_string().contains("PRIVATE_OAUTH"));
    assert!(
        events["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["kind"] == "oauth_request_failed"
                && e["details"]["diagnostics"]["upstream_status"] == 403)
    );
    let browser = c
        .admin(
            "/oauth/browser",
            "POST",
            Some(json!({"name":"Linked in browser"})),
        )
        .await;
    assert_eq!(browser_calls.load(Ordering::SeqCst), 0);
    let url = url::Url::parse(browser["authorization_url"].as_str().unwrap()).unwrap();
    let state = url
        .query_pairs()
        .find(|(k, _)| k == "state")
        .unwrap()
        .1
        .to_string();
    let id = browser["id"].as_str().unwrap();
    let endpoint = format!("{}/api/admin/oauth/browser/{id}/complete", c.base);
    let wrong=c.http.post(&endpoint).header("x-xxgate-csrf","1").json(&json!({"callback_url":"http://localhost:1455/auth/callback?state=wrong&code=PRIVATE_BROWSER_CODE"})).send().await.unwrap();
    assert_eq!(wrong.status(), 400);
    assert_eq!(browser_calls.load(Ordering::SeqCst), 0);
    let callback =
        format!("http://localhost:1455/auth/callback?state={state}&code=PRIVATE_BROWSER_CODE");
    c.admin(
        &format!("/oauth/browser/{id}/complete"),
        "POST",
        Some(json!({"callback_url":callback})),
    )
    .await;
    let duplicate = c
        .http
        .post(&endpoint)
        .header("x-xxgate-csrf", "1")
        .json(&json!({"callback_url":callback}))
        .send()
        .await
        .unwrap();
    assert_eq!(duplicate.status(), 409);
    let result = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let result = c.admin(&format!("/oauth/browser/{id}"), "GET", None).await;
            if result["status"] != "exchanging" {
                break result;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(result["status"], "completed");
    assert_eq!(result["account"]["enabled"], false);
    assert_eq!(browser_calls.load(Ordering::SeqCst), 1);
    assert!(!result.to_string().contains("PRIVATE_BROWSER"));
    let audit = c.admin("/audit?limit=100", "GET", None).await;
    assert!(!audit.to_string().contains("PRIVATE_BROWSER"));
    assert!(!audit.to_string().contains(&state));
    let cancelled = c
        .admin(
            "/oauth/browser",
            "POST",
            Some(json!({"name":"Cancel pending flow"})),
        )
        .await;
    let cancel_id = cancelled["id"].as_str().unwrap();
    c.admin(&format!("/oauth/browser/{cancel_id}"), "DELETE", None)
        .await;
    assert_eq!(
        c.http
            .post(format!(
                "{}/api/admin/oauth/browser/{cancel_id}/complete",
                c.base
            ))
            .header("x-xxgate-csrf", "1")
            .json(&json!({"callback_url":callback}))
            .send()
            .await
            .unwrap()
            .status(),
        410
    );
    c.key = c
        .admin("/keys", "POST", Some(json!({"name":"Integration key"})))
        .await["secret"]
        .as_str()
        .unwrap()
        .to_owned();
    c.admin("/models","PUT",Some(json!({"id":"mock-model","upstream":{"provider":"openai","access_kind":"codex_oauth","model":"mock-upstream"},"enabled":true,"capabilities":{},"version":0}))).await;
    let rates = json!({"input_per_million":"10","cached_input_per_million":"1","output_per_million":"20","image_input_per_million":null,"image_output_per_million":null,"per_image":"1.5"});
    c.admin("/prices","PUT",Some(json!({"version":0,"model":{"provider":"openai","access_kind":"codex_oauth","model":"mock-upstream"},"standard":rates,"fast_multiplier":"2"}))).await;
    let mut account_ids = vec![];
    for name in ["A", "B"] {
        use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
        let claims = URL_SAFE_NO_PAD.encode(json!({"email":format!("{name}@example.com"),"chatgpt_account_id":format!("mock-account-{name}")}).to_string());
        let id_token = format!("header.{claims}.PRIVATE_ID_SIGNATURE");
        let a=c.admin("/accounts","POST",Some(json!({"name":name,"upstream_account_id":format!("mock-account-{name}"),"upstream_base_url":upstream_url,"max_inflight":1,"credentials":{"access_token":format!("PRIVATE_ACCESS_TOKEN_{name}"),"id_token":id_token,"refresh_token":"","expires_at":null}}))).await;
        assert_eq!(a["enabled"], false);
        assert!(!a.to_string().contains("PRIVATE_ACCESS_TOKEN"));
        account_ids.push(a["id"].as_str().unwrap().to_owned());
    }
    let list = c.admin("/accounts", "GET", None).await;
    for (id, email) in account_ids.iter().zip(["A@example.com", "B@example.com"]) {
        let item = list["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["account"]["id"] == *id)
            .unwrap();
        assert_eq!(item["email"], email);
        let detail = c.admin(&format!("/accounts/{id}"), "GET", None).await;
        assert_eq!(detail["email"], email);
        assert!(!detail.to_string().contains("PRIVATE_"));
    }
    assert!(!list.to_string().contains("PRIVATE_"));
    let (a, b) = (&account_ids[0], &account_ids[1]);
    c.admin("/models/sync", "POST", None).await;
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let job = c.admin("/models/sync", "GET", None).await;
            if job["running"] == false {
                assert_eq!(job["completed"], 3);
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let discovered = c.admin("/models/discovered", "GET", None).await;
    assert_eq!(discovered["items"].as_array().unwrap().len(), 4);
    let shared = discovered["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["model"]["model"] == "mock-upstream")
        .unwrap();
    assert_eq!(shared["accounts"].as_array().unwrap().len(), 2);
    assert_eq!(shared["configured_id"], "mock-model");
    let account = gateway
        .scheduler
        .account(Uuid::parse_str(a).unwrap())
        .unwrap();
    assert!(!account.supports(&xxgate_core::types::ModelRef::codex("only-b")));
    assert!(account.supports(&xxgate_core::types::ModelRef::codex("only-a")));
    let synced = account.model_catalog.unwrap().synced_at;
    mock.models_fail.store(true, Ordering::SeqCst);
    assert!(
        gateway
            .sync_models(Uuid::parse_str(a).unwrap())
            .await
            .is_err()
    );
    let account = gateway
        .scheduler
        .account(Uuid::parse_str(a).unwrap())
        .unwrap();
    assert_eq!(account.version, 1);
    assert!(account.supports(&xxgate_core::types::ModelRef::codex("only-a")));
    assert_eq!(account.model_catalog.as_ref().unwrap().synced_at, synced);
    assert!(account.model_catalog.unwrap().error.is_some());
    mock.models_fail.store(false, Ordering::SeqCst);
    // A per-account explicit empty selection must not mean all models.
    for selection in [
        json!(["only-a"]),
        json!([]),
        json!(["mock-upstream", "only-a"]),
    ] {
        let account = c.admin(&format!("/accounts/{a}"), "GET", None).await;
        let changed=c.admin(&format!("/accounts/{a}"),"PUT",Some(json!({"version":account["account"]["version"],"name":"A","max_inflight":1,"models":selection,"models_restricted":true}))).await;
        assert_eq!(changed["models"], selection);
        assert_eq!(changed["models_restricted"], true);
        gateway
            .sync_models(Uuid::parse_str(a).unwrap())
            .await
            .unwrap();
        let current = gateway
            .scheduler
            .account(Uuid::parse_str(a).unwrap())
            .unwrap();
        assert_eq!(
            current.supports(&xxgate_core::types::ModelRef::codex("mock-upstream")),
            selection
                .as_array()
                .unwrap()
                .iter()
                .any(|m| m == "mock-upstream")
        );
    }
    c.admin("/models","PUT",Some(json!({"id":"unavailable-manual-model","upstream":{"provider":"openai","access_kind":"codex_oauth","model":"not-in-any-account"},"enabled":true,"capabilities":{},"version":0}))).await;
    let public = c
        .http
        .get(format!("{}/v1/models", c.base))
        .bearer_auth(&c.key)
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert!(
        public["data"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["id"] == "mock-model")
    );
    assert!(
        !public["data"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["id"] == "unavailable-manual-model")
    );
    c.enabled(a, true).await;
    let mut cfg = c.admin("/settings", "GET", None).await;
    cfg["heartbeat_interval_ms"] = json!(50);
    cfg["queue_timeout_ms"] = json!(5000);
    c.admin("/settings", "PUT", Some(cfg)).await;

    // Stream conversion, aliases, usage, and metadata-only persistence.
    let response = c.response("session-main", "normal", true, "default").await;
    assert_eq!(response.status(), 200);
    assert!(!response.headers().contains_key("content-encoding"));
    let first_id = response.headers()["x-request-id"]
        .to_str()
        .unwrap()
        .to_owned();
    let text = response.text().await.unwrap();
    assert_eq!(text.matches("event: response.completed").count(), 1);
    assert!(text.contains("resp_upstream_"));
    assert!(text.contains("PRIVATE_MODEL_OUTPUT"));
    let first = c.record(&first_id).await;
    assert_eq!(first["request"]["state"], "completed");
    assert_eq!(first["request"]["model"], "mock-model");
    assert_eq!(first["request"]["upstream_model"], "mock-upstream");
    assert_eq!(first["request"]["response_model"], "mock-upstream");
    assert_eq!(first["request"]["upstream_attempts"], 1);
    assert_eq!(first["request"]["stream"], true);
    assert_eq!(first["request"]["binding_generation"], 1);
    assert_eq!(first["request"]["reasoning_effort"], "medium");
    assert_eq!(first["request"]["usage"]["input_tokens"], 1000);
    assert_eq!(
        first["request"]["valuation"]["cny"]
            .as_str()
            .unwrap()
            .parse::<f64>()
            .unwrap(),
        0.0164
    );
    assert!(!first.to_string().contains("PRIVATE_"));
    let capture = mock.captures.lock().await[0].clone();
    assert_eq!(capture.0["session-id"], capture.0["thread-id"]);
    assert_ne!(capture.0["session-id"], "session-main");
    assert_eq!(capture.0["x-client-request-id"], capture.0["thread-id"]);
    assert!(!capture.0.contains_key("cookie"));
    assert!(!capture.0.contains_key("x-codex-turn-state"));
    assert_eq!(capture.1["stream"], true);
    assert_eq!(
        capture.1["input"][0]["content"][0]["text"],
        "PRIVATE_USER_PROMPT"
    );
    let session_a1 = capture.0["session-id"].clone();
    assert_rewrite(
        &first,
        "headers.session-id",
        "session-main",
        session_a1.to_str().unwrap(),
        "rewritten",
    );
    assert_rewrite(
        &first,
        "body.input[0].id",
        "msg_client_input",
        "msg_client_input",
        "unchanged",
    );
    let unary = c
        .response("session-main", "normal", false, "priority")
        .await;
    assert_eq!(unary.status(), 200);
    let unary_id = unary.headers()["x-request-id"].to_str().unwrap().to_owned();
    let unary: Value = unary.json().await.unwrap();
    assert_eq!(unary["status"], "completed");
    assert_eq!(
        c.record(&unary_id).await["request"]["valuation"]["cny"]
            .as_str()
            .unwrap()
            .parse::<f64>()
            .unwrap(),
        0.0328
    );
    assert_eq!(mock.captures.lock().await[1].0["session-id"], session_a1);
    assert_rewrite(
        &c.record(&unary_id).await,
        "headers.session-id",
        "session-main",
        session_a1.to_str().unwrap(),
        "rewritten",
    );

    // A conversation started before the gateway can bring its encrypted history.
    let legacy_input = json!([
        {"type":"reasoning","id":"rs_old","encrypted_content":"PRIVATE_LEGACY_REASONING","summary":[],"internal_chat_message_metadata_passthrough":{"opaque":"PRIVATE_LEGACY_METADATA"}},
        {"type":"compaction","id":"cmp_old","encrypted_content":"PRIVATE_LEGACY_COMPACTION"},
        {"type":"function_call","id":"fc_old","call_id":"call_old","name":"read_file","arguments":"{}","encrypted_function_args":"PRIVATE_LEGACY_ARGS"},
        {"type":"function_call_output","id":"fco_old","call_id":"call_old","output":"PRIVATE_LEGACY_OUTPUT"},
        {"type":"custom_tool_call","id":"ctc_old","call_id":"call_custom_old","name":"apply_patch","input":"PRIVATE_LEGACY_TOOL_INPUT"},
        {"type":"custom_tool_call_output","id":"ctco_old","call_id":"call_custom_old","output":"PRIVATE_LEGACY_TOOL_OUTPUT"}
    ]);
    let before_legacy = mock.calls.load(Ordering::SeqCst);
    let legacy = c.legacy_response(&legacy_input, true).await;
    assert_eq!(legacy.status(), 200);
    let legacy_id = legacy.headers()["x-request-id"]
        .to_str()
        .unwrap()
        .to_owned();
    assert!(legacy.text().await.unwrap().contains("response.completed"));
    assert_eq!(mock.calls.load(Ordering::SeqCst), before_legacy + 1);
    let legacy_record = c.record(&legacy_id).await;
    assert_eq!(legacy_record["request"]["binding_generation"], 1);
    assert_eq!(legacy_record["request"]["upstream_attempts"], 1);
    assert!(!legacy_record.to_string().contains("PRIVATE_"));
    assert!(
        !legacy_record["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|event| event["kind"] == "legacy_context_adopted")
    );
    let legacy_binding =
        Uuid::parse_str(legacy_record["request"]["binding_id"].as_str().unwrap()).unwrap();
    let saved_mappings = store.mappings(legacy_binding).await.unwrap();
    assert_eq!(
        saved_mappings.iter().filter(|m| m.kind == "opaque").count(),
        0
    );
    assert!(
        !serde_json::to_string(&saved_mappings)
            .unwrap()
            .contains("PRIVATE_")
    );
    let legacy_capture = mock.captures.lock().await.last().unwrap().clone();
    assert_eq!(legacy_capture.1["input"], legacy_input);
    for (index, field) in [
        (0, "encrypted_content"),
        (1, "encrypted_content"),
        (2, "encrypted_function_args"),
    ] {
        assert_eq!(
            legacy_capture.1["input"][index][field],
            legacy_input[index][field]
        );
    }
    assert_eq!(
        legacy_capture.1["input"][2]["call_id"],
        legacy_capture.1["input"][3]["call_id"]
    );
    assert_eq!(
        legacy_capture.1["input"][4]["call_id"],
        legacy_capture.1["input"][5]["call_id"]
    );
    assert!(
        saved_mappings
            .iter()
            .all(|m| !matches!(m.kind.as_str(), "item" | "call" | "response"))
    );
    // Reproduce both mapping directions persisted by older gateway versions.
    let binding = store
        .active_bindings()
        .await
        .unwrap()
        .into_iter()
        .find(|b| b.id == legacy_binding)
        .unwrap();
    let mut old_ids = xxgate_core::identity::IdentityMap::new(binding, vec![]);
    assert_ne!(old_ids.outbound("item", "rs_old").unwrap(), "rs_old");
    let old_item_alias = old_ids.inbound("item", "rs_old").unwrap();
    let old_call_alias = old_ids.inbound("call", "call_old").unwrap();
    store
        .save_mappings(legacy_binding, &old_ids.take_pending())
        .await
        .unwrap();
    let mut aliased_legacy_input = legacy_input.clone();
    aliased_legacy_input[0]["id"] = json!(old_item_alias);
    aliased_legacy_input[2]["call_id"] = json!(old_call_alias);
    aliased_legacy_input[3]["call_id"] = json!(old_call_alias);

    // Capacity saturation sticks to A. A disable drains the old generation before B runs.
    c.enabled(b, true).await;
    let mut hold = c.response("session-main", "hold", true, "default").await;
    assert!(hold.chunk().await.unwrap().is_some());
    let queued = c.response("session-main", "normal", true, "default").await;
    let queued_id = queued.headers()["x-request-id"]
        .to_str()
        .unwrap()
        .to_owned();
    wait_queue(&gateway, 1).await;
    let calls = mock.calls.load(Ordering::SeqCst);
    c.enabled(a, false).await;
    assert_eq!(mock.calls.load(Ordering::SeqCst), calls);
    mock.release.add_permits(1);
    assert!(hold.text().await.unwrap().contains("response.completed"));
    assert!(queued.text().await.unwrap().contains("response.completed"));
    let migrated = c.record(&queued_id).await;
    assert_eq!(migrated["request"]["account_id"], *b);
    assert_eq!(migrated["request"]["binding_generation"], 2);
    let session_b = mock.captures.lock().await.last().unwrap().0["session-id"].clone();
    assert_ne!(session_a1, session_b);
    c.enabled(a, true).await;
    c.enabled(b, false).await;
    let third = c.response("session-main", "normal", false, "default").await;
    let third_id = third.headers()["x-request-id"].to_str().unwrap().to_owned();
    assert_eq!(third.json::<Value>().await.unwrap()["status"], "completed");
    assert_eq!(
        c.record(&third_id).await["request"]["binding_generation"],
        3
    );
    let session_a3 = mock.captures.lock().await.last().unwrap().0["session-id"].clone();
    assert_ne!(session_a1, session_a3);
    assert_ne!(session_b, session_a3);

    // Empty pools reject both JSON and streaming requests before an SSE connection opens.
    c.enabled(a, false).await;
    let before_empty = mock.calls.load(Ordering::SeqCst);
    for stream in [false, true] {
        let response = tokio::time::timeout(
            Duration::from_secs(1),
            c.response("session-empty", "normal", stream, "default"),
        )
        .await
        .unwrap();
        assert_eq!(response.status(), 503);
        assert!(
            response.headers()["content-type"]
                .to_str()
                .unwrap()
                .starts_with("application/json")
        );
        let id = response.headers()["x-request-id"]
            .to_str()
            .unwrap()
            .to_owned();
        assert_eq!(
            response.json::<Value>().await.unwrap()["error"]["code"],
            "no_available_account"
        );
        let rejected = c.record(&id).await;
        assert_eq!(rejected["request"]["state"], "rejected");
        assert_eq!(rejected["request"]["upstream_attempts"], 0);
        assert_eq!(rejected["request"]["stream"], stream);
        assert_eq!(rejected["request"]["valuation"]["status"], "not_executed");
        assert_eq!(gateway.scheduler.stats().queued, 0);
    }
    assert_eq!(mock.calls.load(Ordering::SeqCst), before_empty);
    // Capacity waiters still receive heartbeats and observe a shortened deadline.
    c.enabled(a, true).await;
    let mut capacity_hold = c.response("capacity-holder", "hold", true, "default").await;
    assert!(capacity_hold.chunk().await.unwrap().is_some());
    let before = mock.calls.load(Ordering::SeqCst);
    let mut waiting = c
        .response("session-timeout", "normal", true, "default")
        .await;
    let waiting_id = waiting.headers()["x-request-id"]
        .to_str()
        .unwrap()
        .to_owned();
    let heartbeat = waiting.chunk().await.unwrap().unwrap();
    let heartbeat = String::from_utf8_lossy(&heartbeat);
    assert!(heartbeat.starts_with(": xxgate.queue_heartbeat request_id="));
    assert!(!heartbeat.contains("data:") && !heartbeat.contains("event:"));
    tokio::time::sleep(Duration::from_millis(120)).await;
    let mut cfg = c.admin("/settings", "GET", None).await;
    cfg["queue_timeout_ms"] = json!(100);
    c.admin("/settings", "PUT", Some(cfg)).await;
    let ended = waiting.text().await.unwrap();
    assert_eq!(ended.matches("event: response.failed").count(), 1);
    assert!(!ended.contains("response.completed"));
    assert_eq!(mock.calls.load(Ordering::SeqCst), before);
    let record = c.record(&waiting_id).await;
    assert_eq!(record["request"]["error_code"], "queue_timeout");
    assert_eq!(record["request"]["upstream_attempts"], 0);
    assert_eq!(record["request"]["valuation"]["status"], "not_executed");
    assert!(
        record["request"]["config_versions"]
            .as_array()
            .unwrap()
            .len()
            >= 2
    );
    let mut cfg = c.admin("/settings", "GET", None).await;
    cfg["queue_timeout_ms"] = json!(5000);
    c.admin("/settings", "PUT", Some(cfg)).await;
    // Close an actual TCP socket. Dropping an unread client Response can keep an
    // HTTP connection draining in the client's connection pool.
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut cancelled = tokio::net::TcpStream::connect(c.base.trim_start_matches("http://"))
        .await
        .unwrap();
    let payload = json!({"model":"mock-model","input":"cancel","stream":true}).to_string();
    let wire = format!(
        "POST /v1/responses HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {}\r\nsession-id: session-cancel\r\nthread-id: session-cancel\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
        c.key,
        payload.len(),
        payload
    );
    cancelled.write_all(wire.as_bytes()).await.unwrap();
    let mut buf = vec![0; 4096];
    let len = cancelled.read(&mut buf).await.unwrap();
    let headers = String::from_utf8_lossy(&buf[..len]);
    let cancel_id = headers
        .lines()
        .find_map(|line| line.strip_prefix("x-request-id: "))
        .unwrap()
        .to_owned();
    wait_queue(&gateway, 1).await;
    drop(cancelled);
    wait_queue(&gateway, 0).await;
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert_eq!(c.record(&cancel_id).await["request"]["state"], "cancelled");
    mock.release.add_permits(1);
    assert!(
        capacity_hold
            .text()
            .await
            .unwrap()
            .contains("response.completed")
    );
    wait_inflight(&gateway, 0).await;
    c.enabled(a, true).await;

    // HTTP errors and failed/interrupted SSE produce exactly one failure without retrying.
    for behavior in ["http400", "http429", "streamfail", "cut"] {
        let before = mock.calls.load(Ordering::SeqCst);
        let response = c
            .response(&format!("session-{behavior}"), behavior, true, "default")
            .await;
        let id = response.headers()["x-request-id"]
            .to_str()
            .unwrap()
            .to_owned();
        let text = response.text().await.unwrap();
        assert_eq!(
            text.matches("event: response.failed").count(),
            1,
            "{behavior}: {text}"
        );
        assert!(!text.contains("response.completed"));
        assert_eq!(
            text.contains("PRIVATE_UPSTREAM_ERROR_TEXT"),
            behavior != "cut"
        );
        assert_eq!(mock.calls.load(Ordering::SeqCst), before + 1);
        let record = c.record(&id).await;
        assert_eq!(record["request"]["state"], "failed");
        assert!(!record.to_string().contains("PRIVATE_"));
    }
    let rejected = c
        .response("session-http400-json", "http400", false, "default")
        .await;
    assert_eq!(rejected.status(), 400);
    let rejected_id = rejected.headers()["x-request-id"]
        .to_str()
        .unwrap()
        .to_owned();
    assert_eq!(
        rejected.json::<Value>().await.unwrap()["error"]["message"],
        "PRIVATE_UPSTREAM_ERROR_TEXT"
    );
    assert!(
        !c.record(&rejected_id)
            .await
            .to_string()
            .contains("PRIVATE_")
    );
    let mut stalled = c.response("session-idle", "stall", true, "default").await;
    let stall_id = stalled.headers()["x-request-id"]
        .to_str()
        .unwrap()
        .to_owned();
    stalled.chunk().await.unwrap();
    tokio::time::sleep(Duration::from_millis(120)).await;
    let mut cfg = c.admin("/settings", "GET", None).await;
    cfg["sse_idle_timeout_ms"] = json!(100);
    c.admin("/settings", "PUT", Some(cfg)).await;
    assert!(
        stalled
            .text()
            .await
            .unwrap()
            .contains("upstream_idle_timeout")
    );
    assert_eq!(
        c.record(&stall_id).await["request"]["error_code"],
        "upstream_idle_timeout"
    );
    let mut cfg = c.admin("/settings", "GET", None).await;
    cfg["sse_idle_timeout_ms"] = json!(5000);
    c.admin("/settings", "PUT", Some(cfg)).await;
    let image = c.response("session-image", "image", false, "default").await;
    let image_id = image.headers()["x-request-id"].to_str().unwrap().to_owned();
    assert_eq!(
        image.json::<Value>().await.unwrap()["output"][0]["result"],
        "PRIVATE_IMAGE_BYTES"
    );
    let image = c.record(&image_id).await;
    assert_eq!(image["request"]["usage"]["image_count"], 1);
    assert_eq!(
        image["request"]["valuation"]["cny"]
            .as_str()
            .unwrap()
            .parse::<f64>()
            .unwrap(),
        1.5164
    );

    let identity_record_id = identity_contract::verify(&c, &store, &mock).await;
    client_sources_contract::verify(&c, &store, &mock, a, b).await;
    let search_record_id = search_contract::verify(&c, &gateway, &mock, a, b).await;
    let failure = c
        .response("error-center-contract", "http400", false, "default")
        .await;
    assert_eq!(failure.status(), 400);
    let failure_id = failure.headers()["x-request-id"]
        .to_str()
        .unwrap()
        .to_owned();
    failure.bytes().await.unwrap();
    let error_page = c
        .admin(
            &format!("/request-errors?request_id={failure_id}"),
            "GET",
            None,
        )
        .await;
    assert_eq!(error_page["summary"]["total"], 1);
    assert_eq!(error_page["items"][0]["code"], "upstream_invalid_request");
    assert_eq!(error_page["items"][0]["cause"], "invalid_encrypted_content");
    assert!(!error_page.to_string().contains("PRIVATE_"));
    assert!(error_page["items"][0].get("ingress_diagnostics").is_none());
    assert!(error_page["items"][0].get("usage").is_none());
    assert_eq!(
        c.record(&failure_id).await["request"]["upstream_error"]["reason"],
        "invalid_encrypted_content"
    );
    let anonymous = reqwest::Client::new()
        .get(format!("{}/api/admin/request-errors", c.base))
        .send()
        .await
        .unwrap();
    assert_eq!(anonymous.status(), 401);
    for query in [
        "state=completed",
        "upstream_status=999",
        "limit=101",
        "offset=-1",
    ] {
        assert_eq!(
            c.http
                .get(format!("{}/api/admin/request-errors?{query}", c.base))
                .send()
                .await
                .unwrap()
                .status(),
            400
        );
    }
    compaction_contract::verify(&c, &gateway, &mock, a, b).await;
    let routed_record_id = model_routing_contract::verify(&c).await;
    session_concurrency_contract::verify(&c, &gateway, &mock, a, b).await;
    encrypted_reasoning_recovery_contract::verify(&c, &gateway, &mock).await;
    turn_state_contract::verify(&c, &mock).await;

    // Quota exhaustion is sticky until the administrator explicitly reenables the account.
    let id = Uuid::parse_str(a).unwrap();
    let version = gateway.scheduler.account(id).unwrap().version;
    gateway
        .apply_quotas(
            id,
            version,
            &[xxgate_core::quota::QuotaWindow {
                pool: "codex".into(),
                window_minutes: Some(300),
                used_percent: 100.0,
                resets_at: None,
                observed_at: Utc::now(),
                source: "test".into(),
            }],
        )
        .await
        .unwrap();
    assert!(!gateway.scheduler.account(id).unwrap().enabled);
    c.admin(&format!("/accounts/{a}/quota"), "POST", None).await;
    assert!(!gateway.scheduler.account(id).unwrap().enabled);
    let reset_path = format!("/accounts/{a}/reset-credits");
    assert!(c.admin(&reset_path, "GET", None).await["snapshot"].is_null());
    assert_eq!(
        reqwest::Client::new()
            .get(format!("{}/api/admin{reset_path}", c.base))
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    assert_eq!(
        c.http
            .post(format!("{}/api/admin{reset_path}/consume", c.base))
            .json(&json!({}))
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
    let credits = c
        .admin(&format!("{reset_path}/refresh"), "POST", None)
        .await;
    assert_eq!(credits["usable_count"], 3);
    assert_eq!(credits["snapshot"]["credits"].as_array().unwrap().len(), 7);
    assert_eq!(credits["next_credit"]["id"], "early");
    let reject = c
        .http
        .post(format!("{}/api/admin{reset_path}/consume", c.base))
        .header("x-xxgate-csrf", "1")
        .json(&json!({"operation_id":Uuid::new_v4(),"expected_credit_id":"late"}))
        .send()
        .await
        .unwrap();
    assert_eq!(reject.status(), 409);
    assert!(mock.resets.lock().await.calls.is_empty());
    // Invalid query responses preserve the last successful snapshot.
    mock.resets.lock().await.fail_list = true;
    assert!(gateway.collect_reset_credits(id).await.is_err());
    assert_eq!(c.admin(&reset_path, "GET", None).await["usable_count"], 3);
    mock.resets.lock().await.fail_list = false;
    let reset_operation = Uuid::new_v4();
    let payload = json!({"operation_id":reset_operation,"expected_credit_id":"early"});
    let endpoint = format!("{reset_path}/consume");
    let (first_reset, duplicate) = tokio::join!(
        c.admin(&endpoint, "POST", Some(payload.clone())),
        c.admin(&endpoint, "POST", Some(payload.clone()))
    );
    assert_eq!(first_reset["operation"]["result"]["code"], "reset");
    assert_eq!(duplicate["operation"]["result"]["code"], "reset");
    assert_eq!(mock.resets.lock().await.calls.len(), 1);
    assert_eq!(mock.resets.lock().await.calls[0]["credit_id"], "early");
    assert!(!gateway.scheduler.account(id).unwrap().enabled);
    let remaining = c.admin(&reset_path, "GET", None).await;
    assert_eq!(remaining["usable_count"], 2);
    assert_eq!(remaining["next_credit"]["id"], "late");
    mock.resets.lock().await.nothing_next = true;
    let nothing = c
        .admin(
            &endpoint,
            "POST",
            Some(json!({"operation_id":Uuid::new_v4(),"expected_credit_id":"late"})),
        )
        .await;
    assert_eq!(nothing["operation"]["result"]["code"], "nothing_to_reset");
    assert_eq!(c.admin(&reset_path, "GET", None).await["usable_count"], 2);
    // The upstream commits a reset, then returns an unreadable result.
    let uncertain_reset = Uuid::new_v4();
    mock.resets.lock().await.lose_response = true;
    assert!(
        gateway
            .consume_reset(id, uncertain_reset, "late")
            .await
            .is_err()
    );
    let pending = c.admin(&reset_path, "GET", None).await;
    assert_eq!(pending["pending"]["id"], json!(uncertain_reset));
    assert_eq!(
        gateway
            .consume_reset(id, Uuid::new_v4(), "no-expiry")
            .await
            .unwrap_err()
            .code,
        "reset_pending"
    );
    assert_eq!(mock.resets.lock().await.calls.len(), 3);

    c.enabled(a, true).await;
    let denied = c
        .response("session-invalid-auth", "http401", true, "default")
        .await;
    assert!(denied.text().await.unwrap().contains("oauth_invalid"));
    assert!(!gateway.scheduler.account(id).unwrap().enabled);

    // Summaries survive cleanup, request bodies never enter persistent reports.
    let dashboard = c.admin("/dashboard", "GET", None).await;
    assert!(dashboard["summary"]["requests"].as_i64().unwrap() >= 12);
    assert_eq!(dashboard["summary"]["images"], 1);
    let fast = c
        .admin("/dashboard?service_tier=priority", "GET", None)
        .await;
    assert_eq!(fast["summary"]["requests"], 1);
    let none = c
        .admin(
            &format!("/dashboard?key_id={}", Uuid::new_v4()),
            "GET",
            None,
        )
        .await;
    assert_eq!(none["summary"]["requests"], 0);
    let rejected = c
        .http
        .post(format!("{}/v1/responses", c.base))
        .bearer_auth(&c.key)
        .json(&json!({"model":"mock-model","input":"PRIVATE_REJECTED_PROMPT","stream":"invalid"}))
        .send()
        .await
        .unwrap();
    assert_eq!(rejected.status(), 400);
    let rejected_id = rejected.headers()["x-request-id"]
        .to_str()
        .unwrap()
        .to_owned();
    assert_eq!(c.record(&rejected_id).await["request"]["state"], "rejected");
    assert!(
        !c.record(&rejected_id)
            .await
            .to_string()
            .contains("PRIVATE_")
    );
    let ciphertext = store
        .credentials(Uuid::parse_str(a).unwrap())
        .await
        .unwrap();
    assert!(!String::from_utf8_lossy(&ciphertext).contains("PRIVATE_ACCESS_TOKEN"));
    let records = store.requests(&RequestFilter::default()).await.unwrap();
    assert!(!records.to_string().contains("PRIVATE_"));
    wait_inflight(&gateway, 0).await;
    gateway.shutdown.cancel();
    gateway.tasks.close();
    gateway.tasks.wait().await;
    let mut interrupted: xxgate_core::audit::RequestRecord =
        serde_json::from_value(first["request"].clone()).unwrap();
    interrupted.id = Uuid::new_v4();
    interrupted.state = "inflight".into();
    interrupted.finished_at = None;
    interrupted.created_at = Utc::now() - chrono::Duration::days(31);
    interrupted.usage = Default::default();
    interrupted.valuation = None;
    store.begin_request(&interrupted).await.unwrap();
    assert_eq!(store.reconcile_interrupted().await.unwrap(), 1);
    let recovered = store.request_detail(interrupted.id).await.unwrap();
    assert_eq!(recovered["request"]["state"], "interrupted");
    assert_eq!(recovered["request"]["usage"]["complete"], false);
    let totals = store.dashboard(&UsageFilter::default()).await.unwrap();
    let cleaned = store.cleanup(&gateway.settings.current()).await.unwrap();
    assert_eq!(cleaned["requests_deleted"], 1);
    assert!(store.request_detail(interrupted.id).await.is_err());
    assert_eq!(
        store.dashboard(&UsageFilter::default()).await.unwrap()["summary"],
        totals["summary"]
    );
    let restored = Gateway::new(
        store.clone(),
        CredentialCipher::new(&[7; 32]),
        Arc::new(ResponsesIngress),
        Arc::new(CodexProvider),
        |s| Arc::new(HttpTransport::new(s, true)),
    )
    .await
    .unwrap();
    assert!(!restored.scheduler.account(id).unwrap().enabled);
    assert_eq!(store.reconcile_interrupted().await.unwrap(), 0);
    assert_eq!(
        store
            .request_detail(Uuid::parse_str(&routed_record_id).unwrap())
            .await
            .unwrap()["request"]["response_model"],
        "mock-routed-model"
    );
    assert_eq!(
        store
            .request_detail(Uuid::parse_str(&identity_record_id).unwrap())
            .await
            .unwrap()["request"]["ingress_diagnostics"]["identity"]["error"]["code"],
        "identity_conflict"
    );
    assert_eq!(
        store
            .request_detail(Uuid::parse_str(&search_record_id).unwrap())
            .await
            .unwrap()["request"]["valuation"]["cny"],
        "0.12345678"
    );
    // A fresh gateway retains the original request and credit after a crash.
    let retry = restored
        .consume_reset(id, uncertain_reset, "late")
        .await
        .unwrap();
    assert_eq!(
        retry.result.unwrap().code,
        xxgate_core::resets::ResetCode::AlreadyRedeemed
    );
    let reset_calls = &mock.resets.lock().await.calls.clone();
    assert_eq!(reset_calls.len(), 4);
    assert_eq!(reset_calls[2], reset_calls[3]);
    let replay = restored
        .consume_reset(id, reset_operation, "early")
        .await
        .unwrap();
    assert_eq!(
        replay.result.unwrap().code,
        xxgate_core::resets::ResetCode::Reset
    );
    assert_eq!(mock.resets.lock().await.calls.len(), 4);
    let remaining = restored.collect_reset_credits(id).await.unwrap();
    assert_eq!(remaining.next(Utc::now()).unwrap().id, "no-expiry");
    assert_eq!(remaining.credits.len(), 7);

    server_task.abort();
    let (restored_base, restored_server) =
        serve(router(AppState::new(restored.clone(), true, false))).await;
    c.base = restored_base;
    c.admin(
        "/login",
        "POST",
        Some(json!({"password":"xxgate-test-password"})),
    )
    .await;
    c.enabled(a, true).await;
    let continued = c.legacy_response(&legacy_input, false).await;
    assert_eq!(continued.status(), 200);
    let continued_id = continued.headers()["x-request-id"]
        .to_str()
        .unwrap()
        .to_owned();
    assert_eq!(
        continued.json::<Value>().await.unwrap()["status"],
        "completed"
    );
    let continued_record = c.record(&continued_id).await;
    assert_eq!(
        continued_record["request"]["binding_id"],
        legacy_record["request"]["binding_id"]
    );
    assert!(
        !continued_record["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|event| event["kind"] == "legacy_context_adopted")
    );
    let continued_capture = mock.captures.lock().await.last().unwrap().clone();
    assert_eq!(
        continued_capture.0["session-id"],
        legacy_capture.0["session-id"]
    );
    assert_eq!(continued_capture.1["input"], legacy_capture.1["input"]);

    // The gateway forwards encrypted history after account migration too.
    c.enabled(b, true).await;
    c.enabled(a, false).await;
    let before_migration = mock.calls.load(Ordering::SeqCst);
    let forwarded = c.legacy_response(&aliased_legacy_input, false).await;
    assert_eq!(forwarded.status(), 200);
    let forwarded_id = forwarded.headers()["x-request-id"]
        .to_str()
        .unwrap()
        .to_owned();
    assert_eq!(
        forwarded.json::<Value>().await.unwrap()["status"],
        "completed"
    );
    assert_eq!(mock.calls.load(Ordering::SeqCst), before_migration + 1);
    let forwarded_record = c.record(&forwarded_id).await;
    assert_eq!(forwarded_record["request"]["account_id"], *b);
    assert_eq!(forwarded_record["request"]["binding_generation"], 2);
    assert_eq!(forwarded_record["request"]["upstream_attempts"], 1);
    assert!(!forwarded_record.to_string().contains("PRIVATE_"));
    assert_rewrite(
        &forwarded_record,
        "body.input[0].id",
        aliased_legacy_input[0]["id"].as_str().unwrap(),
        "rs_old",
        "alias_restored",
    );
    assert_rewrite(
        &forwarded_record,
        "body.input[2].call_id",
        aliased_legacy_input[2]["call_id"].as_str().unwrap(),
        "call_old",
        "alias_restored",
    );
    assert!(
        !forwarded_record["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|event| event["kind"] == "legacy_context_adopted")
    );
    let forwarded_binding =
        Uuid::parse_str(forwarded_record["request"]["binding_id"].as_str().unwrap()).unwrap();
    assert!(
        !store
            .mappings(forwarded_binding)
            .await
            .unwrap()
            .iter()
            .any(|m| m.kind == "opaque")
    );
    let forwarded_capture = mock.captures.lock().await.last().unwrap().clone();
    assert_eq!(forwarded_capture.1["input"], legacy_input);
    assert_eq!(forwarded_capture.0["chatgpt-account-id"], "mock-account-B");
    assert_ne!(
        forwarded_capture.0["session-id"],
        legacy_capture.0["session-id"]
    );
    for (index, field) in [
        (0, "encrypted_content"),
        (1, "encrypted_content"),
        (2, "encrypted_function_args"),
        (0, "internal_chat_message_metadata_passthrough"),
    ] {
        assert_eq!(
            forwarded_capture.1["input"][index][field],
            legacy_input[index][field]
        );
    }
    assert_eq!(
        forwarded_capture.1["input"][2]["call_id"],
        forwarded_capture.1["input"][3]["call_id"]
    );
    assert_eq!(
        forwarded_capture.1["input"][4]["call_id"],
        forwarded_capture.1["input"][5]["call_id"]
    );
    // List presentation uses the request's historical price, not today's rates.
    c.admin("/prices", "PUT", Some(json!({"version":0,"model":{"provider":"openai","access_kind":"codex_oauth","model":"mock-upstream"},"standard":{"input_per_million":"99","cached_input_per_million":"9","output_per_million":"199","image_input_per_million":null,"image_output_per_million":null,"per_image":"3"},"fast_multiplier":"5"}))).await;
    let listed = c
        .admin(&format!("/requests?id={unary_id}"), "GET", None)
        .await;
    let listed = &listed["items"][0];
    assert_eq!(listed["account_name"], "A");
    assert_eq!(listed["reasoning_effort"], "medium");
    assert_eq!(listed["price"]["standard"]["input_per_million"], "10");
    assert_eq!(listed["price"]["standard"]["output_per_million"], "20");
    assert_eq!(listed["price"]["standard"]["cached_input_per_million"], "1");
    assert_eq!(listed["price"]["fast_multiplier"], "2");
    assert_eq!(listed["valuation"]["cny"], "0.0328");
    assert!(listed.get("ingress_diagnostics").is_none());
    assert!(listed["usage"].get("raw_usage").is_none());
    // Both admin JSON and embedded assets negotiate compression. Public SSE
    // above remains uncompressed even when the caller advertises both codecs.
    for path in [
        "/assets/app.js",
        "/assets/error-center.js",
        "/api/admin/requests?limit=25",
        "/api/admin/request-errors",
    ] {
        let plain = c
            .http
            .get(format!("{}{path}", c.base))
            .header("accept-encoding", "identity")
            .send()
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap();
        for encoding in ["gzip", "br"] {
            let response = c
                .http
                .get(format!("{}{path}", c.base))
                .header("accept-encoding", encoding)
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), 200);
            assert_eq!(response.headers()["content-encoding"], encoding);
            assert!(
                response.headers()["vary"]
                    .to_str()
                    .unwrap()
                    .to_ascii_lowercase()
                    .contains("accept-encoding")
            );
            let encoded = response.bytes().await.unwrap();
            assert!(encoded.len() < plain.len() / 2);
            if encoding == "gzip" {
                assert_eq!(&encoded[..2], &[0x1f, 0x8b]);
            }
        }
    }
    group_contract(&c, &restored, &mock, a, b).await;
    // Deletion revokes access and survives reload while preserving history.
    let before = c.admin(&format!("/accounts/{a}"), "GET", None).await;
    let account_id = Uuid::parse_str(a).unwrap();
    let stale_account = restored.scheduler.account(account_id).unwrap();
    c.admin(&format!("/accounts/{a}"), "DELETE", None).await;
    restored.scheduler.update_account(stale_account);
    assert!(restored.scheduler.account(account_id).is_none());
    assert!(
        !restored
            .store
            .accounts()
            .await
            .unwrap()
            .iter()
            .any(|x| x.id == account_id)
    );
    let after = c
        .admin(&format!("/dashboard?account_id={a}"), "GET", None)
        .await;
    assert_eq!(
        before["statistics"]["summary"]["requests"],
        after["summary"]["requests"]
    );
    for (method, path, body) in [
        ("GET", format!("/accounts/{a}"), None),
        (
            "PUT",
            format!("/accounts/{a}/enabled"),
            Some(json!({"enabled":true})),
        ),
        ("DELETE", format!("/accounts/{a}"), None),
    ] {
        let mut req = c
            .http
            .request(
                method.parse().unwrap(),
                format!("{}/api/admin{path}", c.base),
            )
            .header("x-xxgate-csrf", "1");
        if let Some(body) = body {
            req = req.json(&body);
        }
        assert_eq!(req.send().await.unwrap().status(), 404);
    }
    let created = c
        .admin("/keys", "POST", Some(json!({"name":"delete regression"})))
        .await;
    let key_id = created["key"]["id"].as_str().unwrap();
    let secret = created["secret"].as_str().unwrap();
    c.admin(&format!("/keys/{key_id}"), "DELETE", None).await;
    assert!(
        restored
            .store
            .key_by_hash(&xxgate_core::access::secret_hash(secret))
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        !restored
            .store
            .keys()
            .await
            .unwrap()
            .iter()
            .any(|k| k.id.to_string() == key_id)
    );
    let denied = c
        .http
        .get(format!("{}/v1/models", c.base))
        .bearer_auth(secret)
        .send()
        .await
        .unwrap();
    assert_eq!(denied.status(), 401);
    let enable = c
        .http
        .put(format!("{}/api/admin/keys/{key_id}/enabled", c.base))
        .header("x-xxgate-csrf", "1")
        .json(&json!({"enabled":true}))
        .send()
        .await
        .unwrap();
    assert_eq!(enable.status(), 404);
    restored.shutdown.cancel();
    restored.tasks.close();
    restored.tasks.wait().await;
    restored_server.abort();
    upstream_task.abort();
}

async fn set_account_groups(c: &Client, id: &str, groups: Value) {
    let d = c.admin(&format!("/accounts/{id}"), "GET", None).await;
    let a = &d["account"];
    c.admin(&format!("/accounts/{id}"),"PUT",Some(json!({"version":a["version"],"name":a["name"],"max_inflight":a["max_inflight"],"models":a["models"],"models_restricted":a["models_restricted"],"group_ids":groups}))).await;
}
async fn group_contract(c: &Client, gateway: &Gateway, mock: &Mock, a: &str, b: &str) {
    let groups = c.admin("/groups", "GET", None).await;
    let default = groups["default_group_id"].clone();
    assert_eq!(groups["items"][0]["is_default"], true);
    let red = c
        .admin("/groups", "POST", Some(json!({"name":"Red"})))
        .await;
    let blue = c
        .admin("/groups", "POST", Some(json!({"name":"Blue"})))
        .await;
    let empty = c
        .admin("/groups", "POST", Some(json!({"name":"Empty"})))
        .await;
    let response = c
        .http
        .post(format!("{}/api/admin/groups", c.base))
        .header("x-xxgate-csrf", "1")
        .json(&json!({"name":"red"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 409);
    c.admin(
        &format!("/groups/{}", red["id"].as_str().unwrap()),
        "PUT",
        Some(json!({"name":"Red renamed"})),
    )
    .await;
    let minted = c
        .admin(
            "/keys",
            "POST",
            Some(json!({"name":"group-key","group_id":red["id"]})),
        )
        .await;
    let group_key_id = minted["key"]["id"].as_str().unwrap();
    let mut scoped = Client {
        http: c.http.clone(),
        base: c.base.clone(),
        key: minted["secret"].as_str().unwrap().into(),
    };
    let before = mock.calls.load(Ordering::SeqCst);
    let no_account = scoped
        .response("group-session", "normal", true, "default")
        .await;
    assert_eq!(no_account.status(), 503);
    let rejected_id = no_account.headers()["x-request-id"]
        .to_str()
        .unwrap()
        .to_owned();
    assert_eq!(
        no_account.json::<Value>().await.unwrap()["error"]["code"],
        "no_available_account"
    );
    assert_eq!(
        c.record(&rejected_id).await["request"]["group_id"],
        red["id"]
    );
    assert_eq!(mock.calls.load(Ordering::SeqCst), before);
    let models: Value = scoped
        .http
        .get(format!("{}/v1/models", scoped.base))
        .bearer_auth(&scoped.key)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(models["data"].as_array().unwrap().is_empty());

    set_account_groups(c, a, json!([red["id"], default])).await;
    set_account_groups(c, b, json!([blue["id"]])).await;
    c.enabled(a, true).await;
    c.enabled(b, true).await;
    let good = scoped
        .response("group-session", "normal", false, "default")
        .await;
    assert_eq!(good.status(), 200);
    let first_id = good.headers()["x-request-id"].to_str().unwrap().to_owned();
    assert_eq!(good.json::<Value>().await.unwrap()["status"], "completed");
    let first = c.record(&first_id).await;
    assert_eq!(first["request"]["account_id"], a);
    assert_eq!(first["request"]["group_id"], red["id"]);
    // Changing a Key's group invalidates its old affinity instead of leaking across groups.
    c.admin(
        &format!("/keys/{group_key_id}"),
        "PUT",
        Some(json!({"name":"moved-key","group_id":blue["id"]})),
    )
    .await;
    let moved = scoped
        .response("group-session", "normal", false, "default")
        .await;
    assert_eq!(moved.status(), 200);
    let moved_id = moved.headers()["x-request-id"].to_str().unwrap().to_owned();
    moved.json::<Value>().await.unwrap();
    let moved = c.record(&moved_id).await;
    assert_eq!(moved["request"]["account_id"], b);
    assert_eq!(moved["request"]["group_id"], blue["id"]);
    assert_eq!(moved["request"]["binding_generation"], 2);
    // Saturation queues within Blue. An idle Red account cannot receive Blue traffic.
    let mut hold = scoped.response("blue-hold", "hold", true, "default").await;
    assert!(hold.chunk().await.unwrap().is_some());
    let waiting = scoped
        .response("blue-wait", "normal", true, "default")
        .await;
    let waiting_id = waiting.headers()["x-request-id"]
        .to_str()
        .unwrap()
        .to_owned();
    wait_queue(gateway, 1).await;
    let before = mock.calls.load(Ordering::SeqCst);
    // Moving the only Blue account out wakes the queued request immediately, without an attempt.
    set_account_groups(c, b, json!([red["id"]])).await;
    let failure = tokio::time::timeout(Duration::from_secs(1), waiting.text())
        .await
        .unwrap()
        .unwrap();
    assert!(failure.contains("no_available_account"));
    assert!(!failure.contains("response.completed"));
    assert_eq!(mock.calls.load(Ordering::SeqCst), before);
    assert_eq!(
        c.record(&waiting_id).await["request"]["upstream_attempts"],
        0
    );
    assert_eq!(gateway.scheduler.stats().queued, 0);
    mock.release.add_permits(1);
    assert!(hold.text().await.unwrap().contains("response.completed"));
    wait_inflight(gateway, 0).await;

    // A group referenced only by a key is also protected against deletion.
    c.admin(
        &format!("/keys/{group_key_id}"),
        "PUT",
        Some(json!({"name":"empty-key","group_id":empty["id"]})),
    )
    .await;
    let in_use = c
        .http
        .delete(format!(
            "{}/api/admin/groups/{}",
            c.base,
            empty["id"].as_str().unwrap()
        ))
        .header("x-xxgate-csrf", "1")
        .send()
        .await
        .unwrap();
    assert_eq!(in_use.status(), 409);
    assert_eq!(
        in_use.json::<Value>().await.unwrap()["error"]["code"],
        "group_in_use"
    );
    let absent = scoped
        .response("empty-group", "normal", false, "default")
        .await;
    assert_eq!(absent.status(), 503);
    c.admin(
        &format!("/keys/{group_key_id}"),
        "PUT",
        Some(json!({"name":"blue-key","group_id":blue["id"]})),
    )
    .await;
    c.admin(
        &format!("/groups/{}", empty["id"].as_str().unwrap()),
        "DELETE",
        None,
    )
    .await;
    let invalid = c
        .http
        .post(format!("{}/api/admin/keys", c.base))
        .header("x-xxgate-csrf", "1")
        .json(&json!({"name":"invalid-group","group_id":Uuid::new_v4()}))
        .send()
        .await
        .unwrap();
    assert_eq!(invalid.status(), 400);
    assert!(!gateway.scheduler.stats().paused);
    let default_delete = c
        .http
        .delete(format!(
            "{}/api/admin/groups/{}",
            c.base,
            default.as_str().unwrap()
        ))
        .header("x-xxgate-csrf", "1")
        .send()
        .await
        .unwrap();
    assert_eq!(default_delete.status(), 409);
    // Disabled keys fail authentication, including model discovery.
    c.admin(
        &format!("/keys/{group_key_id}/enabled"),
        "PUT",
        Some(json!({"enabled":false})),
    )
    .await;
    assert_eq!(
        scoped
            .response("disabled-key", "normal", true, "default")
            .await
            .status(),
        401
    );
    // Confirm group membership and key state are persisted independently of the scheduler cache.
    let persisted = gateway.store.accounts().await.unwrap();
    assert_eq!(
        persisted
            .iter()
            .find(|account| account.id.to_string() == a)
            .unwrap()
            .group_ids
            .len(),
        2
    );
    let saved_key = gateway
        .store
        .keys()
        .await
        .unwrap()
        .into_iter()
        .find(|k| k.id.to_string() == group_key_id)
        .unwrap();
    assert_eq!(saved_key.group_id.to_string(), blue["id"].as_str().unwrap());
    assert!(!saved_key.enabled);
    scoped.key.clear();
}
