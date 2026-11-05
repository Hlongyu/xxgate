use super::{ApiResult, AppState};
use axum::{
    Json,
    body::Body,
    extract::{OriginalUri, Path, Request, State},
    http::{HeaderMap, Method},
    middleware::Next,
    response::Response,
};
use serde_json::{Value, json};
use uuid::Uuid;
use xxgate_core::Error;

fn internal(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<std::sync::Arc<crate::cluster::Cluster>, Error> {
    state
        .cluster
        .clone()
        .filter(|c| c.authenticated(headers))
        .ok_or_else(|| {
            Error::new(
                401,
                "internal_auth_required",
                "Cluster authentication is required",
            )
        })
}

pub async fn reload(State(state): State<AppState>, headers: HeaderMap) -> ApiResult<Json<Value>> {
    internal(&state, &headers)?.reload(&state).await?;
    Ok(Json(json!({"ok":true})))
}

pub async fn close_websocket(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    internal(&state, &headers)?;
    state.gateway.transport.close_response_session(id);
    Ok(Json(json!({"ok":true})))
}

/// Runs inside the existing admin authentication/CSRF guard.
pub async fn admin_dispatch(
    State(state): State<AppState>,
    mut request: Request,
    next: Next,
) -> ApiResult<Response> {
    let Some(cluster) = state.cluster.clone() else {
        return Ok(next.run(request).await);
    };
    let uri = request
        .extensions()
        .get::<OriginalUri>()
        .map(|u| u.0.clone())
        .unwrap_or_else(|| request.uri().clone());
    let path = uri
        .path()
        .strip_prefix("/api/admin")
        .unwrap_or(uri.path())
        .to_owned();
    let parts: Vec<_> = path.trim_matches('/').split('/').collect();
    let mut target = None;
    if cluster.router {
        if parts.first() == Some(&"accounts")
            && parts.len() >= 2
            && !(request.method() == Method::GET && parts.len() == 2)
        {
            let id = Uuid::parse_str(parts[1]).map_err(|_| Error::invalid("Invalid account ID"))?;
            target = Some(cluster.store.account_node(id).await?);
        } else if parts.first() == Some(&"oauth") && parts.len() >= 3 {
            let id = Uuid::parse_str(parts[2])
                .map_err(|_| Error::invalid("Invalid authorization ID"))?;
            target = Some(cluster.store.operation_node(id).await?);
        } else if (path == "/accounts" && request.method() == Method::POST)
            || (path.starts_with("/oauth/") && request.method() == Method::POST)
            || matches!(
                path.as_str(),
                "/models/sync" | "/telemetry" | "/metrics" | "/queue"
            )
        {
            target = request
                .headers()
                .get("x-xxgate-node")
                .and_then(|h| h.to_str().ok())
                .map(str::to_owned);
            if path.starts_with("/oauth/") {
                let (head, body) = request.into_parts();
                let bytes = axum::body::to_bytes(body, 256 * 1024)
                    .await
                    .map_err(|_| Error::invalid("Authorization body exceeds limit"))?;
                let input: Value =
                    serde_json::from_slice(&bytes).map_err(|_| Error::invalid("Invalid JSON"))?;
                if let Some(id) = input["account_id"].as_str() {
                    target = Some(
                        cluster
                            .store
                            .account_node(
                                Uuid::parse_str(id)
                                    .map_err(|_| Error::invalid("Invalid account ID"))?,
                            )
                            .await?,
                    );
                }
                request = Request::from_parts(head, Body::from(bytes));
            }
            if target.is_none() {
                target = cluster
                    .store
                    .nodes()
                    .await?
                    .into_iter()
                    .find(|n| n.role == "worker" && n.online)
                    .map(|n| n.id);
            }
            if target.is_none() {
                return Err(Error::new(503, "no_available_node", "No worker is online").into());
            }
        }
    }
    if let Some(target) = target {
        let node = cluster.node(&target).await?;
        let path = uri.path_and_query().map_or(uri.path(), |p| p.as_str());
        let (head, body) = request.into_parts();
        return Ok(cluster
            .forward(&node, head.method, path, head.headers, body, None)
            .await?);
    }
    let shared_change = !matches!(*request.method(), Method::GET | Method::HEAD)
        && (path == "/settings" || path.starts_with("/keys"));
    let response = next.run(request).await;
    if shared_change && response.status().is_success() {
        cluster.reload(&state).await?;
        cluster.broadcast_reload().await;
    }
    // Update advertised availability immediately after local account mutations.
    if !cluster.router && (path.starts_with("/accounts") || path.starts_with("/oauth")) {
        let _ = cluster.publish(&state).await;
    }
    Ok(response)
}
