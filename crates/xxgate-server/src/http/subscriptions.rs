use super::{ApiResult, AppState};
use axum::{
    Json,
    extract::{Path, State},
};
use bytes::Bytes;
use futures::StreamExt;
use http::{HeaderMap, HeaderValue, Method};
use serde_json::{Value, json};
use uuid::Uuid;
use xxgate_codex::provider::subscription;
use xxgate_core::{
    Error, Result,
    accounts::{Account, Credentials},
    protocol::PreparedRequest,
};

async fn query(s: &AppState, a: &Account, c: &Credentials, path: &str) -> Result<Value> {
    let mut url = url::Url::parse("https://chatgpt.com").expect("static URL");
    url.set_path(path);
    if path.ends_with("/subscriptions") {
        url.query_pairs_mut()
            .append_pair("account_id", &a.upstream_account_id);
    }
    let mut headers = HeaderMap::new();
    let mut bearer = HeaderValue::from_str(&format!("Bearer {}", c.access_token))
        .map_err(|_| Error::invalid("Invalid access token"))?;
    bearer.set_sensitive(true);
    headers.insert(http::header::AUTHORIZATION, bearer);
    headers.insert(
        http::header::ACCEPT,
        HeaderValue::from_static("application/json"),
    );
    headers.insert(
        http::header::ORIGIN,
        HeaderValue::from_static("https://chatgpt.com"),
    );
    headers.insert(
        http::header::REFERER,
        HeaderValue::from_static("https://chatgpt.com/"),
    );
    let cancel = s.gateway.shutdown.child_token();
    let future = async {
        let mut r = s
            .gateway
            .transport
            .send_once(
                PreparedRequest {
                    method: Method::GET,
                    url: url.into(),
                    headers,
                    body: Bytes::new(),
                    account_id: Some(a.id),
                    profile_version: a.version,
                    tls_backend: "subscription_browser".into(),
                },
                cancel.clone(),
            )
            .await?;
        if r.headers
            .get("cf-mitigated")
            .is_some_and(|v| v == "challenge")
        {
            return Err(Error::new(
                502,
                "subscription_cloudflare_challenge",
                "套餐查询被 Cloudflare 验证拦截；已保留上次成功记录",
            ));
        }
        if r.status == 401 {
            return Err(Error::new(
                502,
                "subscription_auth_expired",
                "套餐查询授权已失效，请单独刷新 Token 后重试；已保留上次成功记录",
            ));
        }
        if !(200..300).contains(&r.status) {
            return Err(Error::new(
                502,
                "subscription_upstream_error",
                format!("套餐查询失败（上游 HTTP {}）；已保留上次成功记录", r.status),
            ));
        }
        let mut body = Vec::new();
        while let Some(chunk) = r.bytes.next().await {
            let chunk = chunk?;
            if body.len() + chunk.len() > 1024 * 1024 {
                return Err(Error::new(
                    502,
                    "subscription_response_too_large",
                    "上游套餐响应过大；已保留上次成功记录",
                ));
            }
            body.extend_from_slice(&chunk);
        }
        if cancel.is_cancelled() {
            return Err(Error::cancelled());
        }
        serde_json::from_slice(&body).map_err(|_| {
            Error::new(
                502,
                "subscription_invalid_json",
                "上游未返回套餐 JSON，可能需要浏览器验证；已保留上次成功记录",
            )
        })
    };
    let result = tokio::time::timeout(std::time::Duration::from_secs(15), future)
        .await
        .unwrap_or_else(|_| {
            Err(Error::new(
                504,
                "subscription_timeout",
                "套餐查询超时；已保留上次成功记录",
            ))
        });
    cancel.cancel();
    result
}

pub async fn refresh(State(s): State<AppState>, Path(id): Path<Uuid>) -> ApiResult<Json<Value>> {
    // Manual control action: bound concurrency and reject duplicate clicks rather
    // than queuing many slow Cloudflare requests behind the administrator UI.
    let _guard = s.subscription_refresh.try_lock().map_err(|_| {
        Error::new(
            409,
            "subscription_refresh_busy",
            "正在查询套餐信息，请稍后重试",
        )
    })?;
    let a = s
        .gateway
        .scheduler
        .account(id)
        .ok_or_else(Error::not_found)?;
    // Deliberately bypass Gateway::credentials(), which may rotate expired tokens.
    let c = s
        .gateway
        .cipher
        .decrypt(id, &s.gateway.store.credentials(id).await?)?;
    let checked = query(&s, &a, &c, "/backend-api/accounts/check/v4-2023-04-27")
        .await
        .and_then(|v| subscription::accounts_check(&v, &a.upstream_account_id));
    let snapshot = match checked {
        Ok(snapshot) if snapshot.active_until.is_some() || snapshot.plan_type == "free" => snapshot,
        Err(e) if e.code == "subscription_auth_expired" => return Err(e.into()),
        previous => {
            match query(&s, &a, &c, "/backend-api/subscriptions")
                .await
                .and_then(|v| subscription::subscriptions(&v, &a.upstream_account_id))
            {
                Ok(snapshot) => snapshot,
                Err(e) => {
                    // A partially populated accounts/check result must not turn
                    // a failed expiry query into a misleading successful refresh.
                    tracing::warn!(account_id=%id,code=%e.code,"Subscription refresh failed");
                    return Err(match previous {
                        Err(first) if first.code == "subscription_cloudflare_challenge" => first,
                        _ => e,
                    }
                    .into());
                }
            }
        }
    };
    s.gateway.store.save_subscription(&a, &snapshot).await?;
    Ok(Json(json!({"subscription":snapshot})))
}
