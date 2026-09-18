use super::*;

pub(super) async fn verify(c: &Client, mock: &Mock) {
    for stream in [true, false] {
        for behavior in [
            "normal",
            "turn-state-none",
            "turn-state-empty",
            "turn-state-multiple",
            "http400",
            "cut",
        ] {
            let mut request = c
                .http
                .post(format!("{}/v1/responses", c.base))
                .bearer_auth(&c.key)
                .header("session-id", format!("turn-state-{behavior}-{stream}"))
                .header("cookie", "PRIVATE_COOKIE_NOT_RECORDED");
            let client_value = if behavior == "turn-state-empty" {
                ""
            } else {
                "client-opaque+/=<tag>\"&"
            };
            if behavior != "turn-state-none" {
                request = request.header("x-codex-turn-state", client_value);
                if behavior == "turn-state-multiple" {
                    request = request.header("x-codex-turn-state", "client-second-state");
                }
            }
            let response = request.json(&json!({"model":"mock-model","instructions":behavior,"stream":stream,"input":"PRIVATE_TURN_STATE_PROMPT"})).send().await.unwrap();
            let id = response.headers()["x-request-id"]
                .to_str()
                .unwrap()
                .to_owned();
            // Existing Responses response policy is unchanged, including unary calls.
            assert!(!response.headers().contains_key("x-codex-turn-state"));
            response.bytes().await.unwrap();
            let d = c.record(&id).await;
            let r = &d["request"];
            assert_eq!(r["upstream_attempts"], 1);
            assert_eq!(
                r["state"],
                if matches!(behavior, "http400" | "cut") {
                    "failed"
                } else {
                    "completed"
                }
            );
            let client = &r["client_turn_state"];
            let present = behavior != "turn-state-none";
            let count = if !present {
                0
            } else if behavior == "turn-state-multiple" {
                2
            } else {
                1
            };
            assert_eq!(client["present"], present);
            assert_eq!(client["total_values"], count);
            assert_eq!(client["omitted_values"], 0);
            if present {
                assert_eq!(client["values"][0]["value"], client_value);
                assert_eq!(client["values"][0]["encoding"], "utf8");
                assert_eq!(client["values"][0]["truncated"], false);
            }
            if count == 2 {
                assert_eq!(client["values"][1]["value"], "client-second-state");
            }
            let attempt = d["events"]
                .as_array()
                .unwrap()
                .iter()
                .find(|e| e["kind"] == "upstream_attempt_headers")
                .unwrap();
            let upstream = &attempt["details"]["turn_state"];
            assert_eq!(upstream["present"], present);
            assert_eq!(upstream["total_values"], count);
            if present {
                let value = upstream["values"][0]["value"].as_str().unwrap();
                if behavior == "turn-state-empty" {
                    assert_eq!(value, "");
                } else if behavior == "http400" {
                    assert_eq!(value, "error-turn-state");
                } else {
                    assert!(value.starts_with("server-turn-state-"));
                }
            }
            if count == 2 {
                assert_eq!(upstream["values"][1]["value"], "second-server-state");
            }
            let captures = mock.captures.lock().await;
            let last = &captures.last().unwrap().0;
            assert!(!last.contains_key("x-codex-turn-state"));
            assert!(!last.contains_key("cookie"));
            drop(captures);
            assert!(
                !r["ingress_diagnostics"]
                    .to_string()
                    .contains("client-opaque")
            );
            assert!(!d.to_string().contains("PRIVATE_"));
            let list = c.admin(&format!("/requests?id={id}"), "GET", None).await;
            assert!(list["items"][0].get("client_turn_state").is_none());
            assert!(!list.to_string().contains("client-opaque"));
            assert!(!list.to_string().contains("server-turn-state-"));
        }
    }
    // Rejection before dispatch still records the client observation separately
    // from diagnostics that are emitted to ordinary application logs.
    let before = mock.calls.load(Ordering::SeqCst);
    let response = c
        .http
        .post(format!("{}/v1/responses", c.base))
        .bearer_auth(&c.key)
        .header("x-codex-turn-state", "rejected-client-state")
        .json(&json!({"model":"not-configured","input":"PRIVATE_REJECTED_PROMPT"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 400);
    let id = response.headers()["x-request-id"]
        .to_str()
        .unwrap()
        .to_owned();
    response.bytes().await.unwrap();
    let d = c.record(&id).await;
    assert_eq!(d["request"]["upstream_attempts"], 0);
    assert_eq!(
        d["request"]["client_turn_state"]["values"][0]["value"],
        "rejected-client-state"
    );
    assert!(
        !d["request"]["ingress_diagnostics"]
            .to_string()
            .contains("rejected-client-state")
    );
    assert!(!d["events"].to_string().contains("rejected-client-state"));
    assert_eq!(mock.calls.load(Ordering::SeqCst), before);
}
