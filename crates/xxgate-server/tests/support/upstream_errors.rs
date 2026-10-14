use super::*;

pub(super) fn upstream(behavior: &str) -> Option<Response> {
    if !behavior.starts_with("original-error-") {
        return None;
    }
    let error = json!({"code":"unknown_upstream_code","type":"new_error_type","param":"input[7]","message":"Original failure <script>alert(1)</script>"});
    if behavior == "original-error-http" {
        return Some((StatusCode::BAD_REQUEST, Json(json!({"error":error}))).into_response());
    }
    if behavior == "original-error-text" {
        return Some((StatusCode::BAD_GATEWAY, "Proxy temporarily unavailable").into_response());
    }
    let event = match behavior {
        "original-error-top" => {
            json!({"type":"error","code":error["code"],"message":error["message"],"param":error["param"]})
        }
        "original-error-incomplete" => {
            json!({"type":"response.incomplete","response":{"status":"incomplete","error":null,"incomplete_details":{"reason":"max_output_tokens"},"output":[{"text":"DO_NOT_PERSIST_OUTPUT"}]}})
        }
        _ => {
            json!({"type":"response.failed","response":{"status":"failed","error":error,"output":[{"text":"DO_NOT_PERSIST_OUTPUT"}]}})
        }
    };
    Some(
        (
            [("content-type", "text/event-stream")],
            Body::from(frame(event)),
        )
            .into_response(),
    )
}

pub(super) async fn verify(c: &Client) {
    for stream in [false, true] {
        for behavior in [
            "original-error-http",
            "original-error-text",
            "original-error-top",
            "original-error-failed",
            "original-error-incomplete",
        ] {
            let response = c
                .response(&format!("{behavior}-{stream}"), behavior, stream, "default")
                .await;
            let id = response.headers()["x-request-id"]
                .to_str()
                .unwrap()
                .to_owned();
            let body = response.text().await.unwrap();
            if stream {
                assert_eq!(
                    body.matches(if behavior.ends_with("incomplete") {
                        "event: response.incomplete"
                    } else {
                        "event: response.failed"
                    })
                    .count(),
                    1,
                    "{body}"
                );
            }
            let detail = c.record(&id).await;
            assert_eq!(detail["request"]["state"], "failed");
            let original = &detail["request"]["upstream_error"]["original"];
            let fields = &original["fields"];
            assert_eq!(original["truncated"], false);
            if behavior == "original-error-text" {
                assert_eq!(fields["body"], "Proxy temporarily unavailable");
            } else if behavior == "original-error-incomplete" {
                assert_eq!(fields["incomplete_reason"], "max_output_tokens");
                assert_eq!(fields["event_type"], "response.incomplete");
            } else {
                assert_eq!(fields["code"], "unknown_upstream_code");
                assert_eq!(
                    fields["message"],
                    "Original failure <script>alert(1)</script>"
                );
                assert_eq!(fields["param"], "input[7]");
            }
            assert_eq!(
                original["source"],
                if behavior.ends_with("http") || behavior.ends_with("text") {
                    "http"
                } else {
                    "sse"
                }
            );
            assert!(!detail.to_string().contains("DO_NOT_PERSIST_OUTPUT"));
            assert!(!detail["events"].to_string().contains("Original failure"));
            for path in ["/requests?limit=100", "/request-errors?limit=100"] {
                let page = c.admin(path, "GET", None).await;
                assert!(!page.to_string().contains("Original failure"));
                assert!(!page.to_string().contains("Proxy temporarily unavailable"));
            }
        }
    }
}
