use super::*;

pub(super) async fn verify(c: &Client) -> String {
    let mut routed_id = String::new();
    for stream in [true, false] {
        for (behavior, expected) in [
            ("model-route", Some("mock-routed-model")),
            ("model-missing", None),
            ("model-created-only", Some("mock-created-model")),
            ("model-cut", Some("mock-created-model")),
        ] {
            let response = c
                .response("model-observation", behavior, stream, "default")
                .await;
            let id = response.headers()["x-request-id"]
                .to_str()
                .unwrap()
                .to_owned();
            let body = response.text().await.unwrap();
            if behavior == "model-route" {
                if stream {
                    assert!(body.contains("\"model\":\"mock-routed-model\""));
                } else {
                    assert_eq!(
                        serde_json::from_str::<Value>(&body).unwrap()["model"],
                        "mock-routed-model"
                    );
                }
                routed_id = id.clone();
            }
            let detail = c.record(&id).await;
            let r = &detail["request"];
            assert_eq!(r["model"], "mock-model");
            assert_eq!(r["upstream_model"], "mock-upstream");
            assert_eq!(r["response_model"].as_str(), expected);
            assert_eq!(
                r["state"],
                if behavior == "model-cut" {
                    "failed"
                } else {
                    "completed"
                }
            );
            if behavior != "model-cut" {
                // Observation does not change the frozen requested-model price.
                assert_eq!(
                    r["valuation"]["cny"]
                        .as_str()
                        .unwrap()
                        .parse::<rust_decimal::Decimal>()
                        .unwrap(),
                    rust_decimal::Decimal::new(164, 4)
                );
            }
            let list = c.admin(&format!("/requests?id={id}"), "GET", None).await;
            assert!(list["items"][0].get("response_model").is_none());
        }
    }
    routed_id
}
