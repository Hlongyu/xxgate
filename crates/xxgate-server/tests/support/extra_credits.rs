use super::*;
use xxgate_core::{accounts::DisableReason, protocol::ProviderAdapter, quota::QuotaWindow};

pub(super) async fn usage(State(mock): State<Mock>) -> Json<Value> {
    Json(mock.quota_payload.lock().await.clone().unwrap_or_else(|| json!({"rate_limit":{"primary_window":{"used_percent":10,"limit_window_seconds":18000},"secondary_window":{"used_percent":25,"limit_window_seconds":604800}}})))
}

async fn policy(c: &Client, id: &str, enabled: bool) -> Value {
    let a = c.admin(&format!("/accounts/{id}"), "GET", None).await["account"].clone();
    c.admin(&format!("/accounts/{id}"), "PUT", Some(json!({"version":a["version"],"name":a["name"],"models":a["models"],"max_inflight":a["max_inflight"],"use_extra_credits":enabled}))).await
}

pub(super) async fn verify(c: &Client, gateway: &Arc<Gateway>, mock: &Mock, a: &str) {
    let id = Uuid::parse_str(a).unwrap();
    let path = format!("/accounts/{a}/quota");
    assert!(!gateway.scheduler.account(id).unwrap().use_extra_credits);
    let payload = json!({"credits":{"has_credits":true,"unlimited":false,"balance":"25.5"},"rate_limit":{"primary_window":{"used_percent":100,"limit_window_seconds":18000,"reset_at":(Utc::now()+chrono::Duration::hours(5)).timestamp()},"secondary_window":{"used_percent":100,"limit_window_seconds":604800,"reset_at":(Utc::now()+chrono::Duration::days(7)).timestamp()}}});
    *mock.quota_payload.lock().await = Some(payload.clone());
    let result = c.admin(&path, "POST", None).await;
    assert_eq!(result["extra_credits"]["snapshot"]["balance"], "25.5");
    assert_eq!(result["extra_credits"]["available"], true);
    assert!(!gateway.scheduler.account(id).unwrap().enabled); // Default remains opt-out.
    assert_eq!(policy(c, a, true).await["enabled"], true);
    let response = c
        .response("extra-credit-request", "normal", false, "default")
        .await;
    assert_eq!(response.status(), 200);
    response.bytes().await.unwrap();
    wait_inflight(gateway, 0).await;
    assert!(gateway.scheduler.account(id).unwrap().enabled);
    let persisted = PgStore::connect(&std::env::var("TEST_DATABASE_URL").unwrap())
        .await
        .unwrap();
    assert_eq!(
        persisted
            .extra_credits(id)
            .await
            .unwrap()
            .unwrap()
            .balance
            .as_deref(),
        Some("25.5")
    );
    assert!(
        persisted
            .accounts()
            .await
            .unwrap()
            .iter()
            .find(|x| x.id == id)
            .unwrap()
            .use_extra_credits
    );
    // The normal mock request publishes new subscription percentages; exhaust again.
    c.admin(&path, "POST", None).await;
    // Turning the opt-in off applies immediately, and turning it back on resumes.
    let stale_version = gateway.scheduler.account(id).unwrap().version;
    assert_eq!(policy(c, a, false).await["enabled"], false);
    let stale_credits = gateway.store.extra_credits(id).await.unwrap().unwrap();
    gateway
        .apply_quota_observation(id, stale_version, &[], Some(&stale_credits), true)
        .await
        .unwrap();
    assert!(!gateway.scheduler.account(id).unwrap().enabled);
    assert_eq!(policy(c, a, true).await["enabled"], true);
    // A credit-only observation must use saved windows to stop exhausted accounts.
    let mut headers = HeaderMap::new();
    headers.insert("x-codex-credits-has-credits", "false".parse().unwrap());
    headers.insert("x-codex-credits-unlimited", "false".parse().unwrap());
    headers.insert("x-codex-credits-balance", "0".parse().unwrap());
    let observed = CodexProvider.headers(&headers);
    let version = gateway.scheduler.account(id).unwrap().version;
    gateway
        .apply_quota_observation(id, version, &[], observed.extra_credits.as_ref(), false)
        .await
        .unwrap();
    assert!(!gateway.scheduler.account(id).unwrap().enabled);
    c.admin(&path, "POST", None).await;
    assert!(gateway.scheduler.account(id).unwrap().enabled);
    // Missing credits in a full query invalidates an earlier positive snapshot.
    let mut missing = payload.clone();
    missing.as_object_mut().unwrap().remove("credits");
    *mock.quota_payload.lock().await = Some(missing);
    c.admin(&path, "POST", None).await;
    assert!(!gateway.scheduler.account(id).unwrap().enabled);
    *mock.quota_payload.lock().await = Some(payload.clone());
    c.admin(&path, "POST", None).await;
    // Explicit upstream quota rejection cannot be undone by a cached balance.
    let version = gateway.scheduler.account(id).unwrap().version;
    gateway
        .disable_observed(id, version, DisableReason::Quota7dExhausted)
        .await
        .unwrap();
    gateway.reenable_expired_quotas().await.unwrap();
    assert!(!gateway.scheduler.account(id).unwrap().enabled);
    c.admin(&path, "POST", None).await;
    assert!(gateway.scheduler.account(id).unwrap().enabled);
    // Independent model quotas remain binding even when extra credits exist.
    let mut other = QuotaWindow {
        pool: "codex_other".into(),
        window_minutes: Some(10080),
        used_percent: 100.0,
        resets_at: None,
        observed_at: Utc::now(),
        source: "test".into(),
    };
    let version = gateway.scheduler.account(id).unwrap().version;
    gateway
        .apply_quotas(id, version, &[other.clone()])
        .await
        .unwrap();
    assert!(!gateway.scheduler.account(id).unwrap().enabled);
    other.used_percent = 0.0;
    other.observed_at = Utc::now();
    gateway.store.save_quotas(id, &[other]).await.unwrap();
    c.admin(&path, "POST", None).await;
    // Manual and OAuth disables cannot be revived by credit queries or policy edits.
    c.enabled(a, false).await;
    c.admin(&path, "POST", None).await;
    assert_eq!(
        policy(c, a, false).await["disable_reason"],
        "admin_disabled"
    );
    assert_eq!(policy(c, a, true).await["enabled"], false);
    c.enabled(a, true).await;
    let version = gateway.scheduler.account(id).unwrap().version;
    gateway
        .disable_observed(id, version, DisableReason::OauthInvalid)
        .await
        .unwrap();
    c.admin(&path, "POST", None).await;
    assert_eq!(
        gateway.scheduler.account(id).unwrap().disable_reason,
        Some(DisableReason::OauthInvalid)
    );
    // Restore the fixture for the remaining contracts.
    *mock.quota_payload.lock().await = None;
    c.admin(&path, "POST", None).await;
    policy(c, a, false).await;
    c.enabled(a, true).await;
}
