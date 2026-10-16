use super::*;

pub(super) async fn verify(c: &Client, mock: &Mock) {
    for level in ["info", "error", "debug"] {
        let mut cfg = c.admin("/settings", "GET", None).await;
        cfg["log_level"] = json!(level);
        let saved = c.admin("/settings", "PUT", Some(cfg)).await;
        assert_eq!(saved["log_level"], level);
        for behavior in ["normal", "streamfail"] {
            let response = c
                .response(
                    &format!("logs-{level}-{behavior}"),
                    behavior,
                    true,
                    "default",
                )
                .await;
            assert_eq!(response.status(), 200);
            let id = response.headers()["x-request-id"]
                .to_str()
                .unwrap()
                .to_owned();
            response.bytes().await.unwrap();
            let detail = c.record(&id).await;
            let r = &detail["request"];
            assert_eq!(r["request_log_level"], level);
            let full = level == "debug" || behavior == "streamfail";
            assert_eq!(r["detail_level"], if full { "full" } else { "summary" });
            assert_eq!(!r["ingress_diagnostics"].is_null(), full);
            assert_eq!(!r["client_turn_state"].is_null(), full);
            assert_eq!(
                detail["events"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|e| e["kind"] == "upstream_attempt_headers"),
                full
            );
            if behavior == "normal" {
                assert!(r["usage"]["complete"].as_bool().unwrap());
                assert!(!r["usage"]["input_tokens"].is_null());
                assert!(!r["valuation"].is_null());
                assert_eq!(!r["usage"]["raw_usage"].is_null(), full);
            } else {
                assert_eq!(r["state"], "failed");
                assert!(!r["error_code"].is_null());
                assert_eq!(
                    r["upstream_error"]["original"]["fields"]["message"],
                    "PRIVATE_UPSTREAM_ERROR_TEXT"
                );
            }
        }
    }
    // Turning Debug off must not discard diagnostics for an already-running
    // Debug request. The next request observes the new setting.
    let held = c
        .response("logs-start-level", "hold", true, "default")
        .await;
    let id = held.headers()["x-request-id"].to_str().unwrap().to_owned();
    let mut cfg = c.admin("/settings", "GET", None).await;
    cfg["log_level"] = json!("info");
    c.admin("/settings", "PUT", Some(cfg)).await;
    mock.release.add_permits(1);
    held.bytes().await.unwrap();
    let detail = c.record(&id).await;
    assert_eq!(detail["request"]["request_log_level"], "debug");
    assert_eq!(detail["request"]["detail_level"], "full");
    let mut cfg = c.admin("/settings", "GET", None).await;
    cfg["log_level"] = json!("debug");
    c.admin("/settings", "PUT", Some(cfg)).await;
}
