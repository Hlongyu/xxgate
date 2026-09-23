use super::*;

const MESSAGE: &str =
    "This content was flagged for possible cybersecurity risk. PRIVATE_UPSTREAM_DETAIL";

static HELD_POLICY: std::sync::OnceLock<Arc<Semaphore>> = std::sync::OnceLock::new();

pub(super) fn upstream(behavior: &str) -> Option<Response> {
    if !behavior.starts_with("policy-") {
        return None;
    }
    let error = json!({"code":"safety_rejected","message":MESSAGE});
    if behavior == "policy-http" {
        return Some((StatusCode::FORBIDDEN, Json(json!({"error":error}))).into_response());
    }
    let event = if behavior == "policy-top-error" {
        json!({"type":"error","code":"safety_rejected","message":MESSAGE})
    } else {
        json!({"type":"response.failed","response":{"id":"resp_policy","status":"failed","output":[],"error":error}})
    };
    if behavior == "policy-held" {
        let hold = HELD_POLICY
            .get_or_init(|| Arc::new(Semaphore::new(0)))
            .clone();
        let (tx, rx) = mpsc::channel::<std::result::Result<Bytes, Infallible>>(1);
        tokio::spawn(async move {
            tx.send(Ok(frame(json!({"type":"response.created","response":{"id":"resp_policy","status":"in_progress","output":[]}})))).await.unwrap();
            let permit = hold.acquire().await.unwrap();
            permit.forget();
            let _ = tx.send(Ok(frame(event))).await;
        });
        return Some(
            (
                [("content-type", "text/event-stream")],
                Body::from_stream(ReceiverStream::new(rx)),
            )
                .into_response(),
        );
    }
    Some(
        (
            [("content-type", "text/event-stream")],
            Body::from(frame(event)),
        )
            .into_response(),
    )
}

pub(super) async fn verify(c: &Client, mock: &Mock) {
    for stream in [false, true] {
        for behavior in ["policy-http", "policy-top-error", "policy-response-failed"] {
            let before = mock.calls.load(Ordering::SeqCst);
            let response = c
                .response(&format!("{behavior}-{stream}"), behavior, stream, "default")
                .await;
            let id = response.headers()["x-request-id"]
                .to_str()
                .unwrap()
                .to_owned();
            // A stream's HTTP headers have already been sent. Its terminal SSE
            // error must carry the rejection; unary responses can use HTTP 403.
            if !stream {
                assert_eq!(response.status(), 403, "{behavior}");
            }
            let body = response.text().await.unwrap();
            assert!(body.contains("upstream_content_policy_violation"), "{body}");
            assert!(body.contains(MESSAGE), "{body}");
            if stream {
                assert_eq!(body.matches("event: response.failed").count(), 1);
            }
            assert_eq!(mock.calls.load(Ordering::SeqCst), before + 1);
            let detail = c.record(&id).await;
            let request = &detail["request"];
            assert_eq!(request["error_code"], "upstream_content_policy_violation");
            assert_eq!(request["upstream_error"]["reason"], "cybersecurity_risk");
            assert_eq!(request["ingress_diagnostics"]["error"]["status"], 403);
            assert!(
                request["error_message"]
                    .as_str()
                    .unwrap()
                    .contains("cybersecurity risk")
            );
            assert!(!detail.to_string().contains("PRIVATE_"));
            let page = c
                .admin(
                    &format!("/request-errors?request_id={id}&cause=cybersecurity_risk"),
                    "GET",
                    None,
                )
                .await;
            assert_eq!(page["summary"]["total"], 1);
            assert_eq!(
                page["items"][0]["code"],
                "upstream_content_policy_violation"
            );
            assert_eq!(page["items"][0]["cause"], "cybersecurity_risk");
            let account_id = request["account_id"].as_str().unwrap();
            let account = c
                .admin(&format!("/accounts/{account_id}"), "GET", None)
                .await;
            assert_eq!(account["account"]["enabled"], true);
        }
    }
}

pub(super) async fn verify_sessions(c: &Client, gateway: &Arc<Gateway>, mock: &Mock) {
    // This session was rejected by the HTTP classification test above.
    let session = "policy-http-false";
    c.admin("/models", "PUT", Some(json!({"id":"safety-test-alias","upstream":{"provider":"openai","access_kind":"codex_oauth","model":"mock-upstream"},"enabled":true,"capabilities":{},"version":0}))).await;
    let before = mock.calls.load(Ordering::SeqCst);
    for model in ["mock-model", "safety-test-alias"] {
        let response = c.http.post(format!("{}/v1/responses",c.base)).bearer_auth(&c.key)
            .header("session-id",session).header("thread-id","different-thread")
            .json(&json!({"model":model,"instructions":"normal","input":"entirely different input","stream":true})).send().await.unwrap();
        assert_eq!(response.status(), 403);
        let id = response.headers()["x-request-id"]
            .to_str()
            .unwrap()
            .to_owned();
        assert_eq!(
            response.json::<Value>().await.unwrap()["error"]["code"],
            "session_safety_blocked"
        );
        let record = c.record(&id).await;
        assert_eq!(record["request"]["state"], "rejected");
        assert_eq!(record["request"]["upstream_attempts"], 0);
        assert_eq!(
            record["request"]["ingress_diagnostics"]["failure_stage"],
            "safety_policy"
        );
        assert_eq!(
            record["request"]["upstream_error"]["reason"],
            "cybersecurity_risk"
        );
    }
    assert_eq!(mock.calls.load(Ordering::SeqCst), before);

    // Same content in a different session reaches upstream: no content matching.
    let response = c
        .response("policy-separate-session", "policy-http", false, "default")
        .await;
    assert_eq!(
        response.json::<Value>().await.unwrap()["error"]["code"],
        "upstream_content_policy_violation"
    );
    assert_eq!(mock.calls.load(Ordering::SeqCst), before + 1);

    // Session names are scoped to the authenticated gateway key.
    let other_key = c
        .admin(
            "/keys",
            "POST",
            Some(json!({"name":"Safety isolation test"})),
        )
        .await["secret"]
        .as_str()
        .unwrap()
        .to_owned();
    let response = c
        .http
        .post(format!("{}/v1/responses", c.base))
        .bearer_auth(other_key)
        .header("session-id", session)
        .header("thread-id", session)
        .json(&json!({"model":"mock-model","input":"normal","stream":false}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    response.bytes().await.unwrap();
    assert_eq!(mock.calls.load(Ordering::SeqCst), before + 2);

    // Stateless requests never poison a shared empty-session bucket.
    for _ in 0..2 {
        let response=c.http.post(format!("{}/v1/responses",c.base)).bearer_auth(&c.key)
            .json(&json!({"model":"mock-model","instructions":"policy-http","input":"same content","stream":false})).send().await.unwrap();
        assert_eq!(
            response.json::<Value>().await.unwrap()["error"]["code"],
            "upstream_content_policy_violation"
        );
    }
    assert_eq!(mock.calls.load(Ordering::SeqCst), before + 4);

    // A fresh gateway has an empty in-memory map, and must load the durable decision.
    let reloaded = Gateway::new(
        gateway.store.clone(),
        gateway.cipher.clone(),
        Arc::new(ResponsesIngress),
        Arc::new(CodexProvider),
        |s| Arc::new(HttpTransport::new(s, true)),
    )
    .await
    .unwrap();
    let (base, task) = serve(router(AppState::new(reloaded.clone(), true, false))).await;
    let response = c
        .http
        .post(format!("{base}/v1/responses"))
        .bearer_auth(&c.key)
        .header("session-id", session)
        .header("thread-id", "after-restart")
        .json(&json!({"model":"mock-model","input":"different after restart","stream":false}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 403);
    assert_eq!(
        response.json::<Value>().await.unwrap()["error"]["code"],
        "session_safety_blocked"
    );
    assert_eq!(mock.calls.load(Ordering::SeqCst), before + 4);
    reloaded.shutdown.cancel();
    reloaded.tasks.close();
    reloaded.tasks.wait().await;
    task.abort();

    // A queued sibling must recheck the session when its scheduling slot opens.
    wait_inflight(gateway, 0).await;
    let original = c.admin("/settings", "GET", None).await;
    let mut cfg = original.clone();
    cfg["session_max_inflight"] = json!(1);
    c.admin("/settings", "PUT", Some(cfg)).await;
    let before = mock.calls.load(Ordering::SeqCst);
    let held = c
        .response("policy-queued-session", "policy-held", true, "default")
        .await;
    wait_inflight(gateway, 1).await;
    let http = c.http.clone();
    let base = c.base.clone();
    let key = c.key.clone();
    let queued = tokio::spawn(async move {
        http.post(format!("{base}/v1/responses")).bearer_auth(key)
            .header("session-id","policy-queued-session").header("thread-id","queued-sibling")
            .json(&json!({"model":"mock-model","instructions":"normal","input":"queued different input","stream":false})).send().await.unwrap()
    });
    tokio::time::timeout(Duration::from_secs(3), async {
        while gateway.scheduler.stats().queued == 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    HELD_POLICY
        .get_or_init(|| Arc::new(Semaphore::new(0)))
        .add_permits(1);
    assert!(
        held.text()
            .await
            .unwrap()
            .contains("upstream_content_policy_violation")
    );
    let response = queued.await.unwrap();
    assert_eq!(response.status(), 403);
    let id = response.headers()["x-request-id"]
        .to_str()
        .unwrap()
        .to_owned();
    assert_eq!(
        response.json::<Value>().await.unwrap()["error"]["code"],
        "session_safety_blocked"
    );
    assert_eq!(c.record(&id).await["request"]["upstream_attempts"], 0);
    assert_eq!(mock.calls.load(Ordering::SeqCst), before + 1);
    let mut cfg = c.admin("/settings", "GET", None).await;
    cfg["session_max_inflight"] = original["session_max_inflight"].clone();
    c.admin("/settings", "PUT", Some(cfg)).await;

    // A non-policy upstream error does not block the session's next request.
    let response = c
        .response("safety-transient-control", "http400", false, "default")
        .await;
    response.bytes().await.unwrap();
    let response = c
        .response("safety-transient-control", "normal", false, "default")
        .await;
    assert_eq!(response.status(), 200);
    response.bytes().await.unwrap();
}
