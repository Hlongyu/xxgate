use super::*;

async fn configure(c: &Client, id: &str, models: Value, restricted: bool, limit: u32) {
    let a = c.admin(&format!("/accounts/{id}"), "GET", None).await["account"].clone();
    c.admin(
        &format!("/accounts/{id}"),
        "PUT",
        Some(json!({
            "version":a["version"],"name":a["name"],"max_inflight":limit,
            "models":models,"models_restricted":restricted
        })),
    )
    .await;
}

async fn request(
    c: &Client,
    thread: &str,
    model: &str,
    behavior: &str,
    stream: bool,
) -> reqwest::Response {
    c.http.post(format!("{}/v1/responses",c.base)).bearer_auth(&c.key)
        .header("session-id","model-migration-contract").header("thread-id",thread)
        .header("x-codex-turn-state","DO_NOT_FORWARD")
        .json(&json!({"model":model,"instructions":behavior,"input":"PRIVATE_MODEL_MIGRATION_INPUT","stream":stream}))
        .send().await.unwrap()
}

async fn created(response: &mut reqwest::Response) {
    assert_eq!(response.status(), 200);
    tokio::time::timeout(Duration::from_secs(2), async {
        let mut body = String::new();
        while !body.contains("response.created") {
            body.push_str(&String::from_utf8_lossy(
                &response.chunk().await.unwrap().unwrap(),
            ));
        }
    })
    .await
    .unwrap();
}

pub(super) async fn verify(c: &Client, gateway: &Gateway, mock: &Mock, a: &str, b: &str) {
    let original_a = c.admin(&format!("/accounts/{a}"), "GET", None).await["account"].clone();
    let original_b = c.admin(&format!("/accounts/{b}"), "GET", None).await["account"].clone();
    let original_settings = c.admin("/settings", "GET", None).await;
    let mut settings = original_settings.clone();
    settings["session_max_inflight"] = json!(3);
    c.admin("/settings", "PUT", Some(settings)).await;
    c.enabled(a, true).await;
    c.enabled(b, false).await;
    configure(c, a, json!(["mock-upstream"]), true, 10).await;
    configure(c, b, json!(["mock-upstream", "only-b"]), true, 10).await;
    c.admin("/models","PUT",Some(json!({"id":"migration-target","upstream":{"provider":"openai","access_kind":"codex_oauth","model":"only-b"},"enabled":true,"capabilities":{},"version":0}))).await;
    let before = mock.calls.load(Ordering::SeqCst);
    let (mut first, mut second) = tokio::join!(
        request(c, "a", "mock-model", "hold", true),
        request(c, "b", "mock-model", "hold", true)
    );
    tokio::join!(created(&mut first), created(&mut second));
    let old_id = first.headers()["x-request-id"].to_str().unwrap().to_owned();
    wait_inflight(gateway, 2).await;
    c.enabled(b, true).await;
    let mut migrated = request(c, "new-thread", "migration-target", "normal", true).await;
    assert_eq!(migrated.status(), 200);
    let id = migrated.headers()["x-request-id"]
        .to_str()
        .unwrap()
        .to_owned();
    let heartbeat = migrated.chunk().await.unwrap().unwrap();
    assert!(String::from_utf8_lossy(&heartbeat).starts_with(": xxgate.queue_heartbeat"));
    wait_queue(gateway, 1).await;
    assert_eq!(mock.calls.load(Ordering::SeqCst), before + 2);
    mock.release.add_permits(1);
    wait_inflight(gateway, 1).await;
    assert_eq!(mock.calls.load(Ordering::SeqCst), before + 2);
    mock.release.add_permits(1);
    let (first, second, migrated) = tokio::join!(first.text(), second.text(), migrated.text());
    for body in [first, second, migrated] {
        assert!(body.unwrap().contains("response.completed"));
    }
    wait_inflight(gateway, 0).await;
    assert_eq!(mock.calls.load(Ordering::SeqCst), before + 3);
    let old = c.record(&old_id).await;
    let new = c.record(&id).await;
    assert_eq!(old["request"]["account_id"], a);
    assert_eq!(new["request"]["account_id"], b);
    assert_eq!(new["request"]["binding_generation"], 2);
    assert_eq!(new["request"]["upstream_attempts"], 1);
    assert_ne!(old["request"]["binding_id"], new["request"]["binding_id"]);
    assert_eq!(new["bindings"].as_array().unwrap().len(), 2);
    assert_eq!(
        c.admin(&format!("/accounts/{a}"), "GET", None).await["account"]["enabled"],
        true
    );
    let captures = mock.captures.lock().await;
    let old_capture = &captures[captures.len() - 3];
    let new_capture = &captures[captures.len() - 1];
    assert_eq!(new_capture.0["chatgpt-account-id"], "mock-account-B");
    assert_ne!(old_capture.0["session-id"], new_capture.0["session-id"]);
    assert!(!new_capture.0.contains_key("x-codex-turn-state"));
    assert_eq!(new_capture.1["model"], "only-b");
    drop(captures);
    assert!(!new.to_string().contains("PRIVATE_MODEL_MIGRATION_INPUT"));
    // Returning to a model supported by B keeps the new binding, including unary.
    let response = request(c, "a", "mock-model", "normal", false).await;
    assert_eq!(response.status(), 200);
    let same_id = response.headers()["x-request-id"]
        .to_str()
        .unwrap()
        .to_owned();
    assert_eq!(
        response.json::<Value>().await.unwrap()["status"],
        "completed"
    );
    assert_eq!(
        c.record(&same_id).await["request"]["binding_id"],
        new["request"]["binding_id"]
    );
    // Migration must never bypass a subsequent per-session safety decision.
    let rejected = request(c, "policy", "migration-target", "policy-http", false).await;
    assert_eq!(rejected.status(), 403);
    rejected.bytes().await.unwrap();
    let calls = mock.calls.load(Ordering::SeqCst);
    configure(c, b, json!(["only-b"]), true, 10).await;
    let blocked = request(c, "a", "mock-model", "normal", false).await;
    assert_eq!(blocked.status(), 403);
    assert_eq!(
        blocked.json::<Value>().await.unwrap()["error"]["code"],
        "session_safety_blocked"
    );
    assert_eq!(mock.calls.load(Ordering::SeqCst), calls);
    for (id, original) in [(a, original_a), (b, original_b)] {
        configure(
            c,
            id,
            original["models"].clone(),
            original["models_restricted"].as_bool().unwrap(),
            original["max_inflight"].as_u64().unwrap() as u32,
        )
        .await;
        c.enabled(id, original["enabled"].as_bool().unwrap()).await;
    }
    let mut settings = c.admin("/settings", "GET", None).await;
    settings["session_max_inflight"] = original_settings["session_max_inflight"].clone();
    c.admin("/settings", "PUT", Some(settings)).await;
}
