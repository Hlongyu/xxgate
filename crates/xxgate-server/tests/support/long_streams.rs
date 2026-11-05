//! Real local HTTP streams, with Tokio's clock advanced only after each event
//! has crossed the upstream transport, gateway, and downstream HTTP connection.
use super::*;
use futures::StreamExt;

type StreamChunk = std::result::Result<Bytes, std::io::Error>;
type ControlledBody = Arc<Mutex<Option<mpsc::Receiver<StreamChunk>>>>;

async fn controlled_upstream(State(body): State<ControlledBody>) -> Response {
    let rx = body.lock().await.take().expect("one upstream attempt");
    (
        [("content-type", "text/event-stream")],
        Body::from_stream(ReceiverStream::new(rx)),
    )
        .into_response()
}

async fn next_stream_event(rx: &mut mpsc::Receiver<Value>, kind: &str) -> Value {
    loop {
        let event = rx.recv().await.expect("stream ended before expected event");
        if event["type"] == kind {
            return event;
        }
        assert_ne!(
            event["type"], "response.failed",
            "unexpected failure: {event}"
        );
    }
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database; run scripts/test.py long_streams --nocapture"]
async fn long_streams_cross_900_seconds_and_preserve_failure_semantics() {
    let database = std::env::var("TEST_DATABASE_URL").expect("disposable database required");
    // This test runs alongside the full HTTP contract with a different master
    // key and administrator password, so it must own a separate schema.
    let pool = sqlx::PgPool::connect(&database).await.unwrap();
    let schema = format!("long_stream_{}", Uuid::new_v4().simple());
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
        .initialize_admin(&hash_password("long-stream-test").unwrap())
        .await
        .unwrap();
    let body: ControlledBody = Default::default();
    let (upstream_url, upstream_task) = serve(
        Router::new()
            .route("/responses", post(controlled_upstream))
            .route(
                "/models",
                get(|| async {
                    Json(
                        json!({"models":[{"slug":"gpt-6-astra","display_name":"Simulated model"}]}),
                    )
                }),
            )
            .with_state(body.clone()),
    )
    .await;
    let gateway = Gateway::new(
        store.clone(),
        CredentialCipher::new(&[9; 32]),
        Arc::new(ResponsesIngress),
        Arc::new(CodexProvider),
        |settings| Arc::new(HttpTransport::new(settings, true)),
    )
    .await
    .unwrap();
    let (base, server_task) = serve(router(AppState::new(gateway.clone(), true, false))).await;
    let mut c = Client {
        http: reqwest::Client::builder()
            .cookie_store(true)
            .build()
            .unwrap(),
        base,
        key: String::new(),
    };
    c.admin(
        "/login",
        "POST",
        Some(json!({"password":"long-stream-test"})),
    )
    .await;
    c.key = c
        .admin("/keys", "POST", Some(json!({"name":"Local simulation"})))
        .await["secret"]
        .as_str()
        .unwrap()
        .to_owned();
    c.admin("/models", "PUT", Some(json!({"id":"mock-model","upstream":{"provider":"openai","access_kind":"codex_oauth","model":"gpt-6-astra"},"enabled":true,"capabilities":{},"version":0}))).await;
    let account = c.admin("/accounts", "POST", Some(json!({
        "name":"Local simulation", "upstream_account_id":"synthetic-account",
        "upstream_base_url":upstream_url,"max_inflight":1,
        "credentials":{"access_token":"synthetic-token","refresh_token":"","expires_at":null}
    }))).await;
    let account_id = account["id"].as_str().unwrap();
    gateway
        .sync_models(Uuid::parse_str(account_id).unwrap())
        .await
        .unwrap();
    c.enabled(account_id, true).await;
    assert_eq!(gateway.settings.current().sse_idle_timeout_ms, 300_000);

    // External PostgreSQL/socket I/O uses real time. Keep the runtime runnable
    // so paused time never auto-jumps over that I/O; the test controls advances.
    let realtime = std::env::var("XXGATE_STREAM_SIMULATION_REALTIME").as_deref() == Ok("1");
    let clock_guard = if realtime {
        None
    } else {
        tokio::time::pause();
        Some(tokio::spawn(async {
            loop {
                tokio::task::yield_now().await;
            }
        }))
    };
    // An optional real-time run exercises one full 17-minute success stream.
    let scenarios: &[&str] = if realtime {
        &["complete"]
    } else {
        &["complete", "whitespace", "reset", "eof", "idle"]
    };
    for &scenario in scenarios {
        let (tx, rx) = mpsc::channel::<StreamChunk>(4);
        *body.lock().await = Some(rx);
        let response_id = format!("synthetic-{scenario}");
        tx.send(Ok(frame(
            json!({"type":"response.created","response":{"id":response_id,"status":"in_progress"}}),
        )))
        .await
        .unwrap();
        let response = c.http.post(format!("{}/v1/responses", c.base))
            .bearer_auth(&c.key)
            .json(&json!({"model":"mock-model","input":"Local synthetic stream; no real model is called.","stream":true,"reasoning":{"effort":"xhigh"}}))
            .send().await.unwrap();
        assert_eq!(response.status(), 200, "{scenario}");
        let request_id = response.headers()["x-request-id"]
            .to_str()
            .unwrap()
            .to_owned();
        let (events_tx, mut events_rx) = mpsc::channel(4);
        let reader = tokio::spawn(async move {
            let mut stream = response.bytes_stream();
            let mut pending = String::new();
            while let Some(chunk) = stream.next().await {
                pending.push_str(std::str::from_utf8(&chunk.unwrap()).unwrap());
                while let Some(end) = pending.find("\n\n") {
                    let block = pending[..end].to_owned();
                    pending.drain(..end + 2);
                    for line in block.lines() {
                        if let Some(data) = line.strip_prefix("data: ") {
                            let value: Value = serde_json::from_str(data).unwrap();
                            if events_tx.send(value).await.is_err() {
                                return;
                            }
                        }
                    }
                }
            }
        });
        next_stream_event(&mut events_rx, "response.created").await;
        let seconds = if matches!(scenario, "complete" | "whitespace") {
            1020
        } else {
            900
        };
        if scenario == "idle" {
            tokio::time::advance(Duration::from_millis(300_001)).await;
        } else {
            for step in 1..=seconds / 30 {
                if realtime {
                    tokio::time::sleep(Duration::from_secs(30)).await;
                } else {
                    tokio::time::advance(Duration::from_secs(30)).await;
                }
                let delta = if scenario == "whitespace" {
                    " \n\t"
                } else {
                    "synthetic progress"
                };
                let kind = if scenario == "whitespace" {
                    "response.function_call_arguments.delta"
                } else {
                    "response.output_text.delta"
                };
                tx.send(Ok(frame(json!({"type":kind,"response_id":response_id,"delta":delta,"synthetic_step":step})))).await.unwrap();
                let event = next_stream_event(&mut events_rx, kind).await;
                assert_eq!(event["delta"], delta, "{scenario} step {step}");
            }
        }
        match scenario {
            "complete" | "whitespace" => {
                tx.send(Ok(frame(json!({"type":"response.completed","response":{"id":response_id,"status":"completed","output":[],"usage":{"input_tokens":1000,"output_tokens":500,"input_tokens_details":{"cached_tokens":400}}}})))).await.unwrap();
                next_stream_event(&mut events_rx, "response.completed").await;
            }
            "reset" => {
                tx.send(Err(std::io::Error::new(
                    std::io::ErrorKind::ConnectionReset,
                    "synthetic upstream disconnect",
                )))
                .await
                .unwrap();
            }
            _ => {}
        }
        drop(tx);
        if !matches!(scenario, "complete" | "whitespace") {
            let event = next_stream_event(&mut events_rx, "response.failed").await;
            assert_eq!(event["type"], "response.failed");
        }
        reader.await.unwrap();
        let record = c.record(&request_id).await;
        let r = &record["request"];
        let expected_code = match scenario {
            "reset" => Some("transport_failed"),
            "eof" => Some("stream_interrupted"),
            "idle" => Some("upstream_idle_timeout"),
            _ => None,
        };
        assert_eq!(r["error_code"].as_str(), expected_code, "{scenario}: {r}");
        assert_eq!(
            r["state"],
            if expected_code.is_some() {
                "failed"
            } else {
                "completed"
            }
        );
        if expected_code.is_none() {
            assert_eq!(r["usage"]["complete"], true);
            assert_eq!(r["usage"]["input_tokens"], 1000);
            assert_eq!(r["usage"]["output_tokens"], 500);
            assert!(r["total_ms"].as_u64().unwrap() >= 1_020_000);
        } else if scenario != "idle" {
            assert!(r["total_ms"].as_u64().unwrap() >= 900_000);
        }
        println!(
            "LONG_STREAM_SIMULATION {}",
            json!({"scenario":scenario,"clock":if realtime {"real"} else {"virtual"},"elapsed_ms":r["total_ms"],"state":r["state"],"error_code":r["error_code"],"usage_complete":r["usage"]["complete"]})
        );
    }
    if let Some(clock_guard) = clock_guard {
        clock_guard.abort();
        tokio::time::resume();
    }
    gateway.shutdown.cancel();
    server_task.abort();
    upstream_task.abort();
}
