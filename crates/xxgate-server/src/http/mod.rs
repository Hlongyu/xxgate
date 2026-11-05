mod admin;
mod auth;
mod catalog;
mod cluster;
mod groups;
mod oauth;
mod oauth_browser;
mod public;
mod public_models;
mod resets;
mod subscriptions;
mod websocket;

use axum::{
    Json, Router,
    body::Body,
    extract::{DefaultBodyLimit, State},
    http::{HeaderValue, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post, put},
};
use serde_json::{Value, json};
use std::{collections::HashMap, sync::Arc};
use tokio::sync::{Mutex, RwLock};
use xxgate_core::{Error, application::gateway::Gateway};

#[derive(Clone)]
pub struct AppState {
    pub gateway: Arc<Gateway>,
    pub cluster: Option<Arc<crate::cluster::Cluster>>,
    pub(crate) telemetry: crate::telemetry::Telemetry,
    pub allow_http: bool,
    pub secure_cookies: bool,
    pub cleanup: Arc<RwLock<Value>>,
    pub(crate) model_sync: Arc<RwLock<catalog::SyncJob>>,
    pub(crate) oauth: Arc<Mutex<HashMap<uuid::Uuid, oauth::Flow>>>,
    pub(crate) websocket_slots: Arc<tokio::sync::Semaphore>,
    pub(crate) subscription_refresh: Arc<Mutex<()>>,
    pub(crate) login_attempts: Arc<Mutex<Vec<tokio::time::Instant>>>,
}
impl AppState {
    pub async fn visible_accounts(
        &self,
    ) -> xxgate_core::Result<Vec<xxgate_core::accounts::Account>> {
        if let Some(c) = self.cluster.as_ref().filter(|c| c.router) {
            c.store.all_accounts().await
        } else {
            self.gateway.store.accounts().await
        }
    }
    pub async fn account_node(&self, id: uuid::Uuid) -> xxgate_core::Result<String> {
        if let Some(c) = &self.cluster {
            c.store.account_node(id).await
        } else {
            Ok("default".into())
        }
    }
    pub fn new(gateway: Arc<Gateway>, allow_http: bool, secure_cookies: bool) -> Self {
        Self {
            gateway,
            cluster: None,
            telemetry: Default::default(),
            allow_http,
            secure_cookies,
            cleanup: Arc::new(RwLock::new(json!({"state":"pending"}))),
            model_sync: Default::default(),
            oauth: Default::default(),
            login_attempts: Default::default(),
            subscription_refresh: Default::default(),
            websocket_slots: Arc::new(tokio::sync::Semaphore::new(256)),
        }
    }
}

pub struct ApiError(pub Error);
impl From<Error> for ApiError {
    fn from(value: Error) -> Self {
        Self(value)
    }
}
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status =
            StatusCode::from_u16(self.0.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        (
            status,
            Json(json!({"error":{"code":self.0.code,"message":self.0.message}})),
        )
            .into_response()
    }
}
pub type ApiResult<T> = Result<T, ApiError>;

pub fn router(state: AppState) -> Router {
    let admin = Router::new()
        .route("/nodes", get(crate::cluster::nodes))
        .route("/session", get(auth::session))
        .route("/logout", post(auth::logout))
        .route("/dashboard", get(admin::dashboard))
        .route(
            "/accounts",
            get(admin::accounts).post(admin::create_account),
        )
        .route(
            "/accounts/{id}",
            get(admin::account_detail)
                .put(admin::update_account)
                .delete(admin::delete_account),
        )
        .route("/accounts/{id}/enabled", put(admin::enable_account))
        .route("/accounts/{id}/refresh", post(admin::refresh_account))
        .route(
            "/accounts/{id}/subscription/refresh",
            post(subscriptions::refresh),
        )
        .route("/accounts/{id}/quota", post(admin::refresh_quota))
        .route("/accounts/{id}/reset-credits", get(resets::get))
        .route(
            "/accounts/{id}/reset-credits/refresh",
            post(resets::refresh),
        )
        .route(
            "/accounts/{id}/reset-credits/consume",
            post(resets::consume),
        )
        .route("/accounts/{id}/models/sync", post(catalog::sync_account))
        .route("/keys", get(admin::keys).post(admin::create_key))
        .route(
            "/keys/{id}",
            put(admin::update_key).delete(admin::delete_key),
        )
        .route("/keys/{id}/enabled", put(admin::enable_key))
        .route("/groups", get(groups::list).post(groups::create))
        .route("/groups/{id}", put(groups::rename).delete(groups::remove))
        .route("/settings", get(admin::settings).put(admin::save_settings))
        .route("/models", get(admin::models).put(admin::save_model))
        .route("/models/discovered", get(catalog::discovered))
        .route(
            "/models/sync",
            get(catalog::sync_status).post(catalog::sync_all),
        )
        .route("/prices", get(admin::prices).put(admin::save_price))
        .route(
            "/search-price",
            get(admin::search_price).put(admin::save_search_price),
        )
        .route("/requests", get(admin::requests))
        .route("/request-errors", get(admin::request_errors))
        .route("/requests/{id}", get(admin::request_detail))
        .route("/queue", get(admin::queue))
        .route("/audit", get(admin::audit))
        .route("/cleanup", get(admin::cleanup_status).post(admin::cleanup))
        .route("/oauth/device", post(oauth::start))
        .route("/oauth/device/{id}", get(oauth::status))
        .route("/oauth/browser", post(oauth_browser::start))
        .route(
            "/oauth/browser/{id}",
            get(oauth::status).delete(oauth_browser::cancel),
        )
        .route(
            "/oauth/browser/{id}/complete",
            post(oauth_browser::complete),
        )
        .route("/metrics", get(admin::metrics))
        .route("/telemetry", get(admin::telemetry))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            cluster::admin_dispatch,
        ))
        .route_layer(middleware::from_fn_with_state(state.clone(), auth::guard))
        .layer(DefaultBodyLimit::max(256 * 1024));
    Router::new()
        .route("/", get(index))
        .route("/assets/app.js", get(js))
        .route("/assets/runtime-telemetry.js", get(runtime_telemetry_js))
        .route("/assets/cache-metrics.js", get(cache_metrics_js))
        .route("/assets/account-quotas.js", get(account_quotas_js))
        .route("/assets/error-center.js", get(error_center_js))
        .route("/assets/app.css", get(css))
        .route("/healthz", get(health))
        .route("/readyz", get(ready))
        .route("/internal/reload", post(cluster::reload))
        .route("/internal/websocket/{id}", axum::routing::delete(cluster::close_websocket))
        .route("/api/admin/login", post(auth::login).layer(DefaultBodyLimit::max(4096)))
        .nest("/api/admin", admin)
        // Compress the management UI and JSON, including local development.
        // Keep model response streams outside this layer so SSE flushes immediately.
        .layer(tower_http::compression::CompressionLayer::new().quality(
            tower_http::CompressionLevel::Precise(4),
        ))
        .route("/v1/images/generations", post(public::image_generations))
        .route("/images/generations", post(public::image_generations))
        .route("/v1/images/edits", post(public::image_edits))
        .route("/images/edits", post(public::image_edits))
        .route("/v1/responses", post(public::responses).get(websocket::responses))
        .route("/responses", post(public::responses).get(websocket::responses))
        .route("/v1/responses/compact", post(public::compact))
        .route("/responses/compact", post(public::compact))
        .route("/v1/alpha/search", post(public::search))
        .route("/alpha/search", post(public::search))
        .route("/backend-api/codex/alpha/search", post(public::search))
        .route("/v1/search", post(public::search))
        .route("/search", post(public::search))
        .route("/v1/models", get(public::models))
        .route("/models", get(public::models))
        .fallback(|| async { (StatusCode::NOT_FOUND, Json(json!({"error":{"code":"not_found","message":"This endpoint is not supported"}}))) })
        .layer(middleware::from_fn(security_headers))
        .with_state(state)
}
async fn health(State(state): State<AppState>) -> ApiResult<Json<Value>> {
    state.gateway.store.health().await?;
    if state.gateway.scheduler.stats().paused {
        return Err(Error::new(
            503,
            "dispatch_paused",
            "Dispatch is paused after a persistence error",
        )
        .into());
    }
    Ok(Json(
        json!({"status":"ok","version":env!("CARGO_PKG_VERSION")}),
    ))
}
async fn ready(State(state): State<AppState>) -> ApiResult<Json<Value>> {
    let _ = health(State(state.clone())).await?;
    if let Some(cluster) = state.cluster.as_ref().filter(|c| c.router)
        && !cluster
            .store
            .nodes()
            .await?
            .iter()
            .any(|n| n.role == "worker" && n.online && n.accepting)
    {
        return Err(Error::new(
            503,
            "no_ready_worker",
            "No worker is ready to accept new sessions",
        )
        .into());
    }
    Ok(Json(json!({"status":"ready"})))
}
async fn security_headers(request: axum::extract::Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers.insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    headers.insert("referrer-policy", HeaderValue::from_static("no-referrer"));
    headers.insert("x-frame-options", HeaderValue::from_static("DENY"));
    headers.insert("cache-control", HeaderValue::from_static("no-store"));
    headers.insert("content-security-policy", HeaderValue::from_static("default-src 'self'; script-src 'self'; style-src 'self'; img-src 'self' data:; connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'"));
    response
}
async fn index() -> Response {
    (
        [("content-type", "text/html; charset=utf-8")],
        Body::from(include_str!("../../web/index.html")),
    )
        .into_response()
}
async fn js() -> Response {
    (
        [("content-type", "text/javascript; charset=utf-8")],
        Body::from(include_str!("../../web/app.js")),
    )
        .into_response()
}
async fn cache_metrics_js() -> Response {
    (
        [("content-type", "text/javascript; charset=utf-8")],
        Body::from(include_str!("../../web/cache-metrics.js")),
    )
        .into_response()
}
async fn account_quotas_js() -> Response {
    (
        [("content-type", "text/javascript; charset=utf-8")],
        Body::from(include_str!("../../web/account-quotas.js")),
    )
        .into_response()
}
async fn css() -> Response {
    (
        [("content-type", "text/css; charset=utf-8")],
        Body::from(include_str!("../../web/app.css")),
    )
        .into_response()
}
async fn error_center_js() -> Response {
    (
        [("content-type", "text/javascript; charset=utf-8")],
        Body::from(include_str!("../../web/error-center.js")),
    )
        .into_response()
}

async fn runtime_telemetry_js() -> Response {
    (
        [("content-type", "text/javascript; charset=utf-8")],
        include_str!("../../web/runtime-telemetry.js"),
    )
        .into_response()
}
