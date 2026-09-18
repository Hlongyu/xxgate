use super::{ApiResult, AppState};
use axum::{
    Json,
    extract::{Request, State},
    http::{HeaderMap, HeaderValue, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};
use chrono::Utc;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::time::{Duration, Instant};
use uuid::Uuid;
use xxgate_core::{
    Error,
    access::{AdminSession, GatewayKey, random_secret, secret_hash, verify_password},
    audit::AuditEvent,
};

pub fn csrf(headers: &HeaderMap) -> Result<(), Error> {
    if headers.get("x-xxgate-csrf").and_then(|v| v.to_str().ok()) != Some("1") {
        return Err(Error::new(
            403,
            "csrf_required",
            "The administrative CSRF header is required",
        ));
    }
    if headers
        .get("sec-fetch-site")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v == "cross-site")
    {
        return Err(Error::new(
            403,
            "cross_site_request",
            "Cross-site administrative requests are not allowed",
        ));
    }
    Ok(())
}
fn cookie(headers: &HeaderMap) -> Option<&str> {
    headers
        .get("cookie")?
        .to_str()
        .ok()?
        .split(';')
        .find_map(|part| part.trim().strip_prefix("xxgate_session="))
        .filter(|s| s.len() <= 128)
}
pub async fn guard(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Result<Response, super::ApiError> {
    if !matches!(
        *request.method(),
        axum::http::Method::GET | axum::http::Method::HEAD
    ) {
        csrf(request.headers())?;
    }
    let value = cookie(request.headers())
        .ok_or_else(|| Error::new(401, "admin_login_required", "Sign in as administrator"))?;
    state
        .gateway
        .store
        .admin_session(&secret_hash(value))
        .await?
        .ok_or_else(|| {
            Error::new(
                401,
                "admin_login_required",
                "The administrator session has expired",
            )
        })?;
    Ok(next.run(request).await)
}
#[derive(Deserialize)]
pub struct Login {
    password: String,
}
pub async fn login(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<Login>,
) -> ApiResult<Response> {
    csrf(&headers)?;
    {
        let mut attempts = state.login_attempts.lock().await;
        attempts.retain(|at| at.elapsed() < Duration::from_secs(60));
        if attempts.len() >= 10 {
            return Err(Error::new(
                429,
                "login_rate_limited",
                "Too many login attempts; try again in one minute",
            )
            .into());
        }
        attempts.push(Instant::now());
    }
    let hash = state
        .gateway
        .store
        .admin_password_hash()
        .await?
        .ok_or_else(Error::storage)?;
    let valid = tokio::task::spawn_blocking(move || verify_password(&body.password, &hash))
        .await
        .map_err(|_| Error::storage())?;
    if !valid {
        return Err(Error::new(401, "invalid_login", "Incorrect administrator password").into());
    }
    let secret = random_secret(32);
    let session = AdminSession {
        id: Uuid::new_v4(),
        expires_at: Utc::now() + chrono::Duration::hours(12),
    };
    state
        .gateway
        .store
        .put_admin_session(&secret_hash(&secret), &session)
        .await?;
    state
        .gateway
        .store
        .append_event(&AuditEvent::new(
            "admin_login",
            "admin",
            None,
            None,
            json!({"session_id":session.id}),
        ))
        .await?;
    let mut response =
        Json(json!({"authenticated":true,"expires_at":session.expires_at})).into_response();
    response.headers_mut().insert(
        "set-cookie",
        HeaderValue::from_str(&format!(
            "xxgate_session={secret}; HttpOnly; SameSite=Strict; Path=/api/admin; Max-Age=43200{}",
            if state.secure_cookies { "; Secure" } else { "" }
        ))
        .map_err(|_| Error::storage())?,
    );
    Ok(response)
}
pub async fn logout(State(state): State<AppState>, headers: HeaderMap) -> ApiResult<Response> {
    if let Some(value) = cookie(&headers) {
        state
            .gateway
            .store
            .delete_admin_session(&secret_hash(value))
            .await?;
    }
    let mut response = StatusCode::NO_CONTENT.into_response();
    response.headers_mut().insert(
        "set-cookie",
        HeaderValue::from_static(
            "xxgate_session=; HttpOnly; SameSite=Strict; Path=/api/admin; Max-Age=0",
        ),
    );
    Ok(response)
}
pub async fn session() -> Json<Value> {
    Json(json!({"authenticated":true}))
}
pub async fn gateway_key(state: &AppState, headers: &HeaderMap) -> Result<GatewayKey, Error> {
    let secret = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .filter(|s| s.starts_with("sk-") && s.len() <= 256)
        .ok_or_else(|| {
            Error::new(
                401,
                "invalid_api_key",
                "A valid gateway API key is required",
            )
        })?;
    state
        .gateway
        .store
        .key_by_hash(&secret_hash(secret))
        .await?
        .ok_or_else(|| {
            Error::new(
                401,
                "invalid_api_key",
                "The gateway API key is invalid or disabled",
            )
        })
}
