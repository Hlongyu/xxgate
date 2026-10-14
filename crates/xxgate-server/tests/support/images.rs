use super::*;

pub(super) async fn upstream_images(
    State(mock): State<Mock>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    assert_eq!(headers["authorization"], "Bearer PRIVATE_ACCESS_TOKEN_A");
    assert_eq!(headers["accept"], "application/json");
    assert_eq!(body["model"], "gpt-image-2");
    assert!(body.get("store").is_none());
    assert!(body.get("stream").is_none());
    assert!(body.get("instructions").is_none());
    assert!(!headers.contains_key("cookie"));
    assert!(!headers.contains_key("x-codex-turn-state"));
    mock.images.lock().await.push((headers, body.clone()));
    match body["prompt"].as_str().unwrap() {
        "PRIVATE_FAIL" => return (StatusCode::BAD_REQUEST, Json(json!({"error":{"code":"invalid_request_error","message":"PRIVATE_UPSTREAM_IMAGE_ERROR"}}))).into_response(),
        "PRIVATE_INVALID" => return Json(json!({"data":[]})).into_response(),
        _ => {},
    }
    let result = json!({"created":123,"data":[{"b64_json":"PRIVATE_IMAGE_BASE64","revised_prompt":"PRIVATE_REVISED"}],"future":{"keep":true},"usage":{"input_tokens":20,"output_tokens":515,"total_tokens":535,"input_tokens_details":{"text_tokens":18,"image_tokens":2},"output_tokens_details":{"text_tokens":0,"image_tokens":515},"unsafe":"PRIVATE_USAGE"}});
    if body["prompt"] == "PRIVATE_WAIT" {
        let (tx, rx) = mpsc::channel::<Result<Bytes, Infallible>>(1);
        tokio::spawn(async move {
            tokio::select! {
                _ = tx.closed() => {},
                permit = mock.images_release.acquire() => {
                    if let Ok(p) = permit { p.forget(); let _ = tx.send(Ok(Bytes::from(result.to_string()))).await; }
                }
            }
        });
        return Body::from_stream(ReceiverStream::new(rx)).into_response();
    }
    Json(result).into_response()
}

async fn configure(c: &Client, id: &str, models: Value, restricted: bool) {
    let a = c.admin(&format!("/accounts/{id}"), "GET", None).await["account"].clone();
    c.admin(&format!("/accounts/{id}"), "PUT", Some(json!({"version":a["version"],"name":a["name"],"max_inflight":a["max_inflight"],"models":models,"models_restricted":restricted}))).await;
}

fn request(c: &Client, path: &str, body: Value) -> reqwest::RequestBuilder {
    c.http
        .post(format!("{}{path}", c.base))
        .bearer_auth(&c.key)
        .header("user-agent", "codex_cli_rs/0.153.4")
        .header("x-codex-image-turn-id", "image-turn-1")
        .header("cookie", "do-not-forward")
        .header("x-codex-turn-state", "do-not-forward")
        .json(&body)
}

pub(super) async fn verify(
    c: &Client,
    gateway: &Gateway,
    mock: &Mock,
    a: &str,
    b: &str,
    memory_probe: &Arc<memory_contract::Probe>,
) {
    let original = c.admin(&format!("/accounts/{a}"), "GET", None).await["account"].clone();
    c.enabled(a, true).await;
    c.enabled(b, false).await;
    let body =
        json!({"model":"gpt-image-2","prompt":"PRIVATE_PROMPT","quality":"low","size":"1024x1024"});
    configure(c, a, json!([]), true).await;
    assert_eq!(
        request(c, "/v1/images/generations", body.clone())
            .send()
            .await
            .unwrap()
            .status(),
        503
    );
    configure(c, a, json!(["mock-upstream", "gpt-image-2"]), true).await;
    // The stored /models catalog does not advertise Images, but the explicit
    // account allowlist must still permit the independently supported route.
    assert!(
        !original["model_catalog"]["models"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["id"] == "gpt-image-2")
    );
    let models: Value = c
        .http
        .get(format!("{}/v1/models", c.base))
        .bearer_auth(&c.key)
        .header("user-agent", "codex_cli_rs/0.153.4")
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
            .any(|m| m["id"] == "gpt-image-2")
    );
    // Do not fabricate a Codex chat-model capability object for Images.
    assert!(
        !models["models"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["slug"] == "gpt-image-2")
    );
    c.admin("/prices","PUT",Some(json!({"version":0,"model":{"provider":"openai","access_kind":"codex_oauth","model":"gpt-image-2"},"standard":{"input_per_million":"10","cached_input_per_million":"1","output_per_million":"20","image_input_per_million":"30","image_output_per_million":"40","per_image":null},"fast_multiplier":"2"}))).await;
    for path in [
        "/v1/images/generations",
        "/images/generations",
        "/v1/images/edits",
        "/images/edits",
    ] {
        let mut input = body.clone();
        if path.ends_with("edits") {
            input["images"] = json!([{"image_url":"data:image/png;base64,PRIVATE_INPUT_IMAGE"}]);
        }
        let response = request(c, path, input.clone()).send().await.unwrap();
        assert_eq!(response.status(), 200);
        let id = response.headers()["x-request-id"]
            .to_str()
            .unwrap()
            .to_owned();
        let output: Value = response.json().await.unwrap();
        assert_eq!(output["data"][0]["b64_json"], "PRIVATE_IMAGE_BASE64");
        assert_eq!(output["future"]["keep"], true);
        let record = c.record(&id).await;
        assert!(!record.to_string().contains("PRIVATE_"));
        let r = &record["request"];
        assert_eq!(
            r["kind"],
            if path.ends_with("edits") {
                "image_edit"
            } else {
                "image_generation"
            }
        );
        assert_eq!(r["upstream_attempts"], 1);
        assert_eq!(r["stateless"], true);
        assert_eq!(r["usage"]["input_tokens"], 20);
        assert_eq!(r["usage"]["image_count"], 1);
        assert_eq!(r["valuation"]["status"], "priced");
        assert_eq!(
            r["valuation"]["cny"]
                .as_str()
                .unwrap()
                .parse::<rust_decimal::Decimal>()
                .unwrap(),
            rust_decimal::Decimal::new(2084, 5)
        );
        let captures = mock.images.lock().await;
        let (h, sent) = captures.last().unwrap();
        assert_eq!(*sent, input);
        assert_ne!(h["x-codex-image-turn-id"], "image-turn-1");
    }
    for (prompt, status) in [("PRIVATE_FAIL", 400), ("PRIVATE_INVALID", 502)] {
        let mut input = body.clone();
        input["prompt"] = json!(prompt);
        let before = mock.images.lock().await.len();
        let response = request(c, "/v1/images/generations", input)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), status);
        let id = response.headers()["x-request-id"]
            .to_str()
            .unwrap()
            .to_owned();
        response.bytes().await.unwrap();
        assert_eq!(mock.images.lock().await.len(), before + 1);
        assert_no_private_outside_original(&c.record(&id).await);
    }
    let response = request(
        c,
        "/v1/responses",
        json!({"model":"gpt-image-2","input":"hi"}),
    )
    .send()
    .await
    .unwrap();
    assert_eq!(response.status(), 400);
    assert_eq!(
        c.http
            .post(format!("{}/v1/images/generations", c.base))
            .json(&body)
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    let response = c
        .http
        .post(format!("{}/v1/images/edits", c.base))
        .bearer_auth(&c.key)
        .header("content-type", "multipart/form-data; boundary=test")
        .body("test")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 415);
    // An image already dispatched must survive pressure while its JSON response arrives.
    let mut held = body.clone();
    held["prompt"] = json!("PRIVATE_WAIT");
    let pending = tokio::spawn(request(c, "/v1/images/generations", held).send());
    wait_inflight(gateway, 1).await;
    memory_probe.0.store(95, Ordering::SeqCst);
    assert!(gateway.memory_admission.check(true).is_err());
    mock.images_release.add_permits(1);
    let completed = pending.await.unwrap().unwrap();
    assert_eq!(completed.status(), 200);
    assert_eq!(
        completed.json::<Value>().await.unwrap()["data"][0]["b64_json"],
        "PRIVATE_IMAGE_BASE64"
    );
    memory_probe.0.store(50, Ordering::SeqCst);
    wait_inflight(gateway, 0).await;
    // A disconnect releases the same capacity lease as Responses requests.
    let mut waiting = body.clone();
    waiting["prompt"] = json!("PRIVATE_WAIT");
    use tokio::io::AsyncWriteExt;
    let mut socket = tokio::net::TcpStream::connect(c.base.trim_start_matches("http://"))
        .await
        .unwrap();
    let body = waiting.to_string();
    let wire = format!(
        "POST /v1/images/generations HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
        c.key,
        body.len(),
        body
    );
    socket.write_all(wire.as_bytes()).await.unwrap();
    wait_inflight(gateway, 1).await;
    drop(socket);
    wait_inflight(gateway, 0).await;
    configure(
        c,
        a,
        original["models"].clone(),
        original["models_restricted"].as_bool().unwrap(),
    )
    .await;
}
