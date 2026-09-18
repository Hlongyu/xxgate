use super::*;

async fn concurrency(c: &Client, id: &str, limit: u32) {
    let mut a = c.admin(&format!("/accounts/{id}"), "GET", None).await["account"].clone();
    a["max_inflight"] = json!(limit);
    let update = json!({"version":a["version"],"name":a["name"],"max_inflight":a["max_inflight"],"models":a["models"],"models_restricted":a["models_restricted"]});
    c.admin(&format!("/accounts/{id}"), "PUT", Some(update))
        .await;
}

async fn session_limit(c: &Client, limit: u32) {
    let mut cfg = c.admin("/settings", "GET", None).await;
    cfg["session_max_inflight"] = json!(limit);
    let saved = c.admin("/settings", "PUT", Some(cfg)).await;
    assert_eq!(saved["session_max_inflight"], limit);
}

async fn request(c: &Client, thread: &str, behavior: &str) -> reqwest::Response {
    c.http
        .post(format!("{}/v1/responses", c.base))
        .bearer_auth(&c.key)
        .header("user-agent", "codex_cli_rs/0.153.4")
        .header("session-id", "session-cap-contract")
        .header("thread-id", thread)
        .json(&json!({"model":"mock-model","instructions":behavior,"input":"PRIVATE_CONCURRENCY_INPUT","stream":true,"prompt_cache_key":"shared-cache"}))
        .send().await.unwrap()
}

async fn created(response: &mut reqwest::Response) {
    assert_eq!(response.status(), 200);
    tokio::time::timeout(Duration::from_secs(2), async {
        let mut data = String::new();
        while !data.contains("response.created") {
            data.push_str(&String::from_utf8_lossy(
                &response.chunk().await.unwrap().unwrap(),
            ));
        }
    })
    .await
    .unwrap();
}

pub(super) async fn verify(c: &Client, gateway: &Gateway, mock: &Mock, a: &str, b: &str) {
    let a_enabled = gateway
        .scheduler
        .account(Uuid::parse_str(a).unwrap())
        .unwrap()
        .enabled;
    let b_enabled = gateway
        .scheduler
        .account(Uuid::parse_str(b).unwrap())
        .unwrap()
        .enabled;
    c.enabled(a, true).await;
    c.enabled(b, false).await;
    let account_limit =
        c.admin(&format!("/accounts/{a}"), "GET", None).await["account"]["max_inflight"]
            .as_u64()
            .unwrap() as u32;
    let original = c.admin("/settings", "GET", None).await;
    assert_eq!(original["session_max_inflight"], 2);
    concurrency(c, a, 10).await;
    session_limit(c, 2).await;
    let before = mock.calls.load(Ordering::SeqCst);
    let (mut first, mut second) = tokio::join!(
        request(c, "thread-a", "hold"),
        request(c, "thread-b", "hold")
    );
    tokio::join!(created(&mut first), created(&mut second));
    wait_inflight(gateway, 2).await;
    assert_eq!(mock.calls.load(Ordering::SeqCst), before + 2);
    let first_id = first.headers()["x-request-id"].to_str().unwrap().to_owned();
    let second_id = second.headers()["x-request-id"]
        .to_str()
        .unwrap()
        .to_owned();

    // The account has spare capacity, but the session has reached its cap.
    let mut third = request(c, "thread-c", "normal").await;
    let third_id = third.headers()["x-request-id"].to_str().unwrap().to_owned();
    let heartbeat = third.chunk().await.unwrap().unwrap();
    let heartbeat = String::from_utf8_lossy(&heartbeat);
    assert!(heartbeat.starts_with(": xxgate.queue_heartbeat request_id="));
    assert!(!heartbeat.contains("data:") && !heartbeat.contains("event:"));
    let fourth = request(c, "thread-a", "normal").await;
    let fourth_id = fourth.headers()["x-request-id"]
        .to_str()
        .unwrap()
        .to_owned();
    wait_queue(gateway, 2).await;
    assert_eq!(mock.calls.load(Ordering::SeqCst), before + 2);
    let queue = c.admin("/queue", "GET", None).await;
    for request in queue["queue"]["requests"].as_array().unwrap() {
        assert_eq!(request["session_inflight"], 2);
        assert_eq!(request["session_max_inflight"], 2);
        assert!(!request["thread_id"].as_str().unwrap().is_empty());
    }

    // A live increase admits another thread without releasing the held requests.
    session_limit(c, 3).await;
    assert!(third.text().await.unwrap().contains("response.completed"));
    wait_queue(gateway, 1).await;
    wait_inflight(gateway, 2).await;
    assert_eq!(mock.calls.load(Ordering::SeqCst), before + 3);
    // The second request for thread-a still cannot overtake its first request.
    mock.release.add_permits(2);
    let (first_body, second_body, fourth_body) =
        tokio::join!(first.text(), second.text(), fourth.text());
    for body in [first_body, second_body, fourth_body] {
        assert!(body.unwrap().contains("response.completed"));
    }
    wait_inflight(gateway, 0).await;
    assert_eq!(mock.calls.load(Ordering::SeqCst), before + 4);
    let mut binding = None;
    for id in [&first_id, &second_id, &third_id, &fourth_id] {
        let detail = c.record(id).await;
        let request = &detail["request"];
        assert_eq!(request["state"], "completed");
        assert_eq!(request["account_id"], a);
        assert_eq!(request["binding_generation"], 1);
        assert_eq!(request["upstream_attempts"], 1);
        assert_eq!(detail["bindings"].as_array().unwrap().len(), 1);
        if let Some(binding) = &binding {
            assert_eq!(&request["binding_id"], binding);
        } else {
            assert!(!request["binding_id"].is_null());
            binding = Some(request["binding_id"].clone());
        }
    }
    {
        let captures = mock.captures.lock().await;
        let current = &captures[captures.len() - 4..];
        assert_eq!(current[0].0["session-id"], current[1].0["session-id"]);
        assert_ne!(current[0].0["thread-id"], current[1].0["thread-id"]);
    }
    session_limit(c, original["session_max_inflight"].as_u64().unwrap() as u32).await;
    concurrency(c, a, account_limit).await;
    c.enabled(a, a_enabled).await;
    c.enabled(b, b_enabled).await;
}
