use super::*;

#[derive(Default)]
pub(super) struct SearchMock {
    captures: Vec<(HeaderMap, Value, String)>,
}

pub(super) async fn upstream_search(
    State(mock): State<Mock>,
    headers: HeaderMap,
    uri: axum::http::Uri,
    Json(body): Json<Value>,
) -> Response {
    assert_eq!(headers["authorization"], "Bearer PRIVATE_ACCESS_TOKEN_A");
    assert_eq!(headers["chatgpt-account-id"], "mock-account-A");
    assert_eq!(headers["accept"], "application/json");
    assert_eq!(headers["version"], "0.153.4");
    mock.search
        .lock()
        .await
        .captures
        .push((headers, body.clone(), uri.to_string()));
    if let Some(status) = body["test_status"].as_u64() {
        return (StatusCode::from_u16(status as u16).unwrap(), Json(json!({"error":{"message":"PRIVATE_SEARCH_ERROR","type":"search_error"},"future":"preserved"}))).into_response();
    }
    if body["test_mode"] == "invalid" {
        return (StatusCode::OK, "invalid json").into_response();
    }
    if body["test_mode"] == "large" {
        return Json(json!({"output":"x".repeat(2048)})).into_response();
    }
    let content = Bytes::from_static(br#"{ "output": "PRIVATE_SEARCH_RESULTS", "encrypted_output": "PRIVATE_SEARCH_CIPHERTEXT", "results": [{"future_type":true}], "future": {"keep":true} }"#);
    let mut response = if body["test_mode"] == "wait" {
        let (tx, rx) = mpsc::channel::<Result<Bytes, Infallible>>(1);
        tokio::spawn(async move {
            tokio::select! {
                _=tx.closed()=>{},
                permit=mock.search_release.acquire()=>{
                    if let Ok(p)=permit {p.forget();let _=tx.send(Ok(content)).await;}
                }
            }
        });
        Body::from_stream(ReceiverStream::new(rx)).into_response()
    } else {
        Body::from(content).into_response()
    };
    response
        .headers_mut()
        .insert("content-type", "application/json".parse().unwrap());
    response
        .headers_mut()
        .insert("x-request-id", "official-search-request".parse().unwrap());
    response
}

async fn search(c: &Client, body: Value) -> reqwest::Response {
    c.http
        .post(format!(
            "{}/v1/alpha/search?feature=test&feature=second",
            c.base
        ))
        .bearer_auth(&c.key)
        .header("OpenAI-Beta", "test-beta")
        .header("Version", "untrusted-client-version")
        .json(&body)
        .send()
        .await
        .unwrap()
}

pub(super) async fn verify(c: &Client, gateway: &Gateway, mock: &Mock, a: &str, b: &str) -> String {
    c.enabled(a, true).await;
    c.enabled(b, false).await;
    let body = json!({"id":"search-session","model":"mock-model","commands":{"search_query":[{"q":"PRIVATE_SEARCH_QUERY_ONE"},{"q":"PRIVATE_SEARCH_QUERY_TWO"}]},"input":[{"type":"reasoning","id":"rs_search","encrypted_content":"PRIVATE_SEARCH_CONTEXT"}],"settings":{"external_web_access":true},"future_field":{"keep":true},"service_tier":"priority"});
    let unpriced = search(c, body.clone()).await;
    assert_eq!(unpriced.status(), 200);
    let unpriced_id = unpriced.headers()["x-request-id"]
        .to_str()
        .unwrap()
        .to_owned();
    assert!(unpriced.text().await.unwrap().starts_with("{ \"output\""));
    let record = c.record(&unpriced_id).await;
    assert_eq!(record["request"]["kind"], "search");
    assert_eq!(record["request"]["stream"], false);
    assert_eq!(record["request"]["usage"]["search_calls"], 1);
    assert_eq!(record["request"]["valuation"]["status"], "unpriced_search");
    assert!(record["request"]["usage"]["input_tokens"].is_null());
    assert!(!record.to_string().contains("PRIVATE_"));
    let capture = mock.search.lock().await.captures.last().unwrap().clone();
    for field in ["input", "commands", "id", "future_field", "settings"] {
        assert_eq!(capture.1[field], body[field]);
    }
    assert_eq!(capture.1["model"], "mock-upstream");
    for field in ["client_metadata", "store", "stream"] {
        assert!(capture.1.get(field).is_none());
    }
    assert_eq!(capture.0["openai-beta"], "test-beta");
    assert_eq!(capture.0["session-id"], capture.0["thread-id"]);
    assert_ne!(
        capture.0["session-id"].to_str().unwrap(),
        body["id"].as_str().unwrap()
    );
    assert_eq!(capture.2, "/alpha/search?feature=test&feature=second");
    let price = c.admin("/search-price", "GET", None).await;
    let price = c
        .admin(
            "/search-price",
            "PUT",
            Some(json!({"version":price["version"],"per_call":"0.12345678"})),
        )
        .await;
    let stale = c
        .http
        .put(format!("{}/api/admin/search-price", c.base))
        .header("x-xxgate-csrf", "1")
        .json(&json!({"version":0,"per_call":"1"}))
        .send()
        .await
        .unwrap();
    assert_eq!(stale.status(), 409);
    let mut waiting_body = body.clone();
    waiting_body["test_mode"] = json!("wait");
    let pending = search(c, waiting_body);
    tokio::pin!(pending);
    let changed = tokio::select! {
        _=&mut pending=>panic!("Search returned before its body completed"),
        ()=wait_inflight(gateway,1)=>{
            // Wait for the upstream to observe dispatch; the selected price is now frozen.
            tokio::time::timeout(Duration::from_secs(3),async {
                while mock.search.lock().await.captures.len()<2 {tokio::task::yield_now().await;}
            }).await.unwrap();
            c.admin("/search-price","PUT",Some(json!({"version":price["version"],"per_call":"0.5"}))).await
        }
    };
    mock.search_release.add_permits(1);
    let blocked = pending.await;
    let blocked_id = blocked.headers()["x-request-id"]
        .to_str()
        .unwrap()
        .to_owned();
    assert_eq!(
        blocked.json::<Value>().await.unwrap()["encrypted_output"],
        "PRIVATE_SEARCH_CIPHERTEXT"
    );
    let billed = c.record(&blocked_id).await;
    assert_eq!(billed["request"]["valuation"]["cny"], "0.12345678");
    assert_eq!(billed["request"]["valuation"]["items"][0]["quantity"], 1);
    assert_eq!(billed["request"]["search_price"]["per_call"], "0.12345678");
    assert_eq!(
        billed["request"]["upstream_request_id"],
        "official-search-request"
    );
    let next = search(c, body.clone()).await;
    let next_id = next.headers()["x-request-id"].to_str().unwrap().to_owned();
    next.bytes().await.unwrap();
    assert_eq!(
        c.record(&next_id).await["request"]["valuation"]["cny"],
        "0.5"
    );
    for status in [400, 429, 500] {
        let before = mock.search.lock().await.captures.len();
        let mut failed_body = body.clone();
        failed_body["test_status"] = json!(status);
        let failed = search(c, failed_body).await;
        assert_eq!(failed.status().as_u16(), status);
        let failed_id = failed.headers()["x-request-id"]
            .to_str()
            .unwrap()
            .to_owned();
        assert_eq!(
            failed.json::<Value>().await.unwrap()["error"]["message"],
            "PRIVATE_SEARCH_ERROR"
        );
        let failed_record = c.record(&failed_id).await;
        assert_eq!(failed_record["request"]["usage"]["search_calls"], 0);
        assert_eq!(failed_record["request"]["valuation"]["cny"], "0");
        assert_eq!(failed_record["request"]["upstream_attempts"], 1);
        assert!(!failed_record.to_string().contains("PRIVATE_"));
        assert_eq!(mock.search.lock().await.captures.len(), before + 1);
    }
    let mut invalid_body = body.clone();
    invalid_body["test_mode"] = json!("invalid");
    let invalid = search(c, invalid_body).await;
    assert_eq!(invalid.status(), 502);
    let invalid_id = invalid.headers()["x-request-id"]
        .to_str()
        .unwrap()
        .to_owned();
    assert_eq!(
        c.record(&invalid_id).await["request"]["valuation"]["cny"],
        "0"
    );
    let free = c
        .admin(
            "/search-price",
            "PUT",
            Some(json!({"version":changed["version"],"per_call":"0"})),
        )
        .await;
    let response = search(c, body.clone()).await;
    let free_id = response.headers()["x-request-id"]
        .to_str()
        .unwrap()
        .to_owned();
    response.bytes().await.unwrap();
    let free_record = c.record(&free_id).await;
    assert_eq!(free_record["request"]["valuation"]["status"], "priced");
    assert_eq!(free_record["request"]["valuation"]["cny"], "0");
    c.admin(
        "/search-price",
        "PUT",
        Some(json!({"version":free["version"],"per_call":null})),
    )
    .await;
    let searches = c.admin("/requests?kind=search", "GET", None).await;
    assert_eq!(searches["total"], 8);
    assert!(
        searches["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|r| r["kind"] == "search")
    );
    let summary = c.admin("/dashboard", "GET", None).await;
    assert_eq!(summary["summary"]["search_calls"], 4);
    assert_eq!(
        summary["summary"]["search_cny"]
            .as_str()
            .unwrap()
            .parse::<rust_decimal::Decimal>()
            .unwrap(),
        rust_decimal::Decimal::new(62345678, 8)
    );
    assert_eq!(
        c.record(&blocked_id).await["request"]["valuation"]["cny"],
        "0.12345678"
    );
    // An in-flight Search observes tightened deadlines and releases its permit.
    let mut waiting_body = body.clone();
    waiting_body["test_mode"] = json!("wait");
    let timeout = search(c, waiting_body);
    tokio::pin!(timeout);
    tokio::select! {
        _=&mut timeout=>panic!("Search returned before its body completed"),
        ()=wait_inflight(gateway,1)=>{
            tokio::time::timeout(Duration::from_secs(3),async {
                while mock.search.lock().await.captures.len()<9 {tokio::task::yield_now().await;}
            }).await.unwrap();
            let mut settings=c.admin("/settings","GET",None).await;
            settings["sse_idle_timeout_ms"]=json!(100);
            c.admin("/settings","PUT",Some(settings)).await;
        }
    }
    let timeout = timeout.await;
    assert_eq!(timeout.status(), 504);
    let timeout_id = timeout.headers()["x-request-id"]
        .to_str()
        .unwrap()
        .to_owned();
    timeout.bytes().await.unwrap();
    assert_eq!(
        c.record(&timeout_id).await["request"]["valuation"]["cny"],
        "0"
    );
    wait_inflight(gateway, 0).await;
    let mut settings = c.admin("/settings", "GET", None).await;
    let limit = settings["sse_event_limit_bytes"].clone();
    settings["sse_idle_timeout_ms"] = json!(5000);
    settings["sse_event_limit_bytes"] = json!(1024);
    c.admin("/settings", "PUT", Some(settings)).await;
    let mut large = body;
    large["test_mode"] = json!("large");
    let too_large = search(c, large).await;
    assert_eq!(too_large.status(), 502);
    assert_eq!(
        too_large.json::<Value>().await.unwrap()["error"]["code"],
        "search_response_too_large"
    );
    let mut settings = c.admin("/settings", "GET", None).await;
    settings["sse_event_limit_bytes"] = limit;
    c.admin("/settings", "PUT", Some(settings)).await;
    blocked_id
}
