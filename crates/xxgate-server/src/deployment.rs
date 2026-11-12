//! Planned worker handoff. The old listener becomes a forwarding bridge only
//! after every admitted response and background task has finished and its
//! database ownership connection has closed. No generation is ever replayed.
use crate::{
    cluster::{Cluster, WS_HEADER},
    http::{ApiResult, AppState},
};
use axum::{
    Json,
    body::Body,
    extract::{Request, State},
    http::HeaderMap,
    middleware::Next,
    response::{IntoResponse, Response},
};
use http_body_util::BodyExt;
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::Connection;
use std::{
    sync::{
        Arc,
        atomic::{AtomicU8, AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::{Mutex, OwnedRwLockReadGuard, RwLock, Semaphore};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use xxgate_core::Error;
use xxgate_postgres::cluster::Node;

#[derive(Default)]
enum Destination {
    #[default]
    Local,
    Remote(Box<Node>),
}

pub struct Deployment {
    gate: Arc<RwLock<Destination>>,
    phase: AtomicU8,
    active: AtomicUsize,
    pub(crate) ownership: Mutex<Option<sqlx::PgConnection>>,
    pub(crate) stop: CancellationToken,
    pending: Semaphore,
}

impl Deployment {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            pending: Semaphore::new(256),
            gate: Default::default(),
            phase: Default::default(),
            active: Default::default(),
            ownership: Default::default(),
            stop: Default::default(),
        })
    }

    pub fn phase(&self) -> &'static str {
        match self.phase.load(Ordering::Acquire) {
            0 => "serving",
            1 => "draining",
            2 => "transferring",
            3 => "forwarding",
            _ => "failed",
        }
    }

    pub fn planned(&self) -> bool {
        self.phase.load(Ordering::Acquire) != 0
    }

    pub fn active(&self) -> usize {
        self.active.load(Ordering::Acquire)
    }

    pub(crate) async fn release(&self) -> xxgate_core::Result<()> {
        if let Some(connection) = self.ownership.lock().await.take() {
            connection.close().await.map_err(|_| Error::storage())?;
        }
        Ok(())
    }
}

// Clones in the response stream and WebSocket callback retain ONE admission.
#[derive(Clone)]
pub(crate) struct Admission {
    _lease: Arc<Lease>,
}
struct Lease {
    deployment: Arc<Deployment>,
    _gate: OwnedRwLockReadGuard<Destination>,
}
impl Drop for Lease {
    fn drop(&mut self) {
        self.deployment.active.fetch_sub(1, Ordering::AcqRel);
    }
}

pub async fn admission(
    State(state): State<AppState>,
    mut request: Request,
    next: Next,
) -> Response {
    let path = request.uri().path();
    if matches!(
        path,
        "/healthz" | "/readyz" | "/internal/deployment" | "/internal/deployment/handoff"
    ) {
        return next.run(request).await;
    }
    // Count responses on routers too, including upgraded WebSockets, so an
    // operator can prove an old entry is empty before retiring it.
    let deployment = state.deployment.clone();
    let pending = if deployment.planned() {
        match deployment.pending.try_acquire() {
            Ok(permit) => Some(permit),
            Err(_) => {
                return crate::http::ApiError(Error::new(
                    503,
                    "handoff_capacity",
                    "Worker handoff wait capacity is exhausted",
                ))
                .into_response();
            }
        }
    } else {
        None
    };
    let guard = deployment.gate.clone().read_owned().await;
    drop(pending);
    let target = match &*guard {
        Destination::Remote(node) => Some(node.clone()),
        Destination::Local => None,
    };
    if state.gateway.shutdown.is_cancelled() && target.is_none() {
        return crate::http::ApiError(Error::new(
            503,
            "handoff_incomplete",
            "Worker ownership is being transferred",
        ))
        .into_response();
    }
    deployment.active.fetch_add(1, Ordering::AcqRel);
    let lease = Admission {
        _lease: Arc::new(Lease {
            deployment,
            _gate: guard,
        }),
    };
    let response = if let Some(node) = target {
        let cluster = state
            .cluster
            .as_ref()
            .expect("handoff requires cluster mode");
        let trusted = cluster.authenticated(request.headers());
        if request.uri().path().starts_with("/internal/") && !trusted {
            return crate::http::ApiError(Error::new(
                401,
                "internal_auth_required",
                "Cluster authentication is required",
            ))
            .into_response();
        }
        if !trusted {
            request.headers_mut().remove(crate::cluster::REQUEST_HEADER);
        }
        let websocket = trusted
            .then(|| {
                request
                    .headers()
                    .get(WS_HEADER)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| Uuid::parse_str(v).ok())
            })
            .flatten();
        let (parts, body) = request.into_parts();
        let path = parts
            .uri
            .path_and_query()
            .map_or(parts.uri.path(), |p| p.as_str());
        match cluster
            .forward(&node, parts.method, path, parts.headers, body, websocket)
            .await
        {
            Ok(response) => response,
            Err(error) => crate::http::ApiError(error).into_response(),
        }
    } else {
        request.extensions_mut().insert(lease.clone());
        next.run(request).await
    };
    track(response, lease)
}

fn track(response: Response, lease: Admission) -> Response {
    let (parts, body) = response.into_parts();
    // Frame stream preserves trailers as well as SSE data and backpressure.
    let stream = futures::stream::unfold((body, lease), |(mut body, lease)| async {
        body.frame().await.map(|frame| (frame, (body, lease)))
    });
    Response::from_parts(parts, Body::new(http_body_util::StreamBody::new(stream)))
}

fn internal(state: &AppState, headers: &HeaderMap) -> xxgate_core::Result<Arc<Cluster>> {
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

pub async fn status(State(state): State<AppState>, headers: HeaderMap) -> ApiResult<Json<Value>> {
    let cluster = internal(&state, &headers)?;
    Ok(Json(
        json!({"protocol":1,"node_id":cluster.store.node_id(),"boot_id":cluster.boot_id,"role":if cluster.router {"router"} else {"worker"},"phase":state.deployment.phase(),"active":state.deployment.active()}),
    ))
}

#[derive(Deserialize)]
pub struct Handoff {
    expected_boot_id: Uuid,
    successor_url: String,
    expected_successor_boot_id: Uuid,
    #[serde(default = "drain_timeout")]
    drain_timeout_secs: u64,
}
fn drain_timeout() -> u64 {
    300
}

pub async fn handoff(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<Handoff>,
) -> ApiResult<Json<Value>> {
    let cluster = internal(&state, &headers)?;
    if state.gateway.shutdown.is_cancelled() {
        return Err(Error::conflict().into());
    }
    if cluster.router || cluster.boot_id != input.expected_boot_id {
        return Err(Error::new(
            409,
            "handoff_owner_changed",
            "Handoff requires the expected worker incarnation",
        )
        .into());
    }
    let url = url::Url::parse(&input.successor_url)
        .map_err(|_| Error::invalid("Invalid successor origin"))?;
    if !["http", "https"].contains(&url.scheme())
        || url.host_str().is_none()
        || url.path() != "/"
        || url.query().is_some()
        || url.fragment().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
        || !(1..=3600).contains(&input.drain_timeout_secs)
    {
        return Err(Error::invalid(
            "Successor must be an HTTP(S) origin; drain timeout must be 1–3600 seconds",
        )
        .into());
    }
    if state.deployment.ownership.lock().await.is_none() {
        return Err(Error::new(
            409,
            "handoff_unmanaged",
            "This process does not hold a managed ownership lock",
        )
        .into());
    }
    let current = cluster.node(cluster.store.node_id()).await?;
    if current.url.trim_end_matches('/') == input.successor_url.trim_end_matches('/') {
        return Err(Error::invalid("Successor must use a different endpoint").into());
    }
    let successor = cluster.deployment_status(&input.successor_url).await?;
    if successor["phase"] != "standby"
        || successor["protocol"] != 1
        || successor["node_id"] != cluster.store.node_id()
        || successor["boot_id"] != input.expected_successor_boot_id.to_string()
    {
        return Err(Error::new(
            409,
            "candidate_mismatch",
            "Successor must hold the standby slot for this node and expected boot",
        )
        .into());
    }
    if state
        .deployment
        .phase
        .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return Err(Error::conflict().into());
    }
    tokio::spawn(async move {
        if let Err(error) = transfer(&state, &cluster, current, input).await {
            tracing::error!(code=%error.code, "worker handoff failed; old listener retained");
            // Before cancellation we can resume. After cancellation, ownership
            // stays protected until all tasks finish; never restart old tasks.
            if !state.gateway.shutdown.is_cancelled() {
                state.deployment.phase.store(0, Ordering::Release);
            } else {
                state.deployment.phase.store(4, Ordering::Release);
            }
        }
    });
    Ok(Json(json!({"status":"draining"})))
}

async fn transfer(
    state: &AppState,
    cluster: &Cluster,
    current: Node,
    input: Handoff,
) -> xxgate_core::Result<()> {
    let mut guard = tokio::time::timeout(
        Duration::from_secs(input.drain_timeout_secs),
        state.deployment.gate.write(),
    )
    .await
    .map_err(|_| {
        Error::new(
            409,
            "drain_timeout",
            "Drain timed out; continuing the current release",
        )
    })?;
    // OAuth browser state is process-local. Refuse while a user is authorizing
    // rather than silently losing a live verifier during a planned release.
    if state.oauth.lock().await.values().any(|f| {
        f.expires > tokio::time::Instant::now()
            && matches!(f.value["status"].as_str(), Some("pending" | "exchanging"))
    }) {
        return Err(Error::new(
            409,
            "oauth_in_progress",
            "Finish pending authorization before handing off this worker",
        ));
    }
    state.deployment.phase.store(2, Ordering::Release);
    // Keep advertising the bridge while the old tasks stop and the successor
    // loads its state. A fresh boot's registration fences this heartbeat.
    let keepalive_stop = CancellationToken::new();
    let _keepalive_guard = keepalive_stop.clone().drop_guard();
    let keepalive = tokio::spawn({
        let stop = keepalive_stop.clone();
        let store = cluster.store.clone();
        let current = current.clone();
        async move {
            loop {
                tokio::select! { _=stop.cancelled()=>break, _=tokio::time::sleep(Duration::from_millis(500))=>{} }
                let mut runtime = current.runtime.clone();
                runtime["deployment"] = json!("transferring");
                if let Err(error) = store
                    .heartbeat(current.boot_id, current.accepting, runtime)
                    .await
                {
                    if error.code == "node_ownership_lost" {
                        break;
                    }
                    tracing::warn!(code=%error.code, "handoff bridge heartbeat failed");
                }
            }
        }
    });
    state.gateway.shutdown.cancel();
    state.gateway.tasks.close();
    state.gateway.tasks.wait().await;
    state.deployment.release().await?;
    // The write guard continues holding new requests until the successor is
    // registered AND answers a direct authenticated readiness probe.
    loop {
        if state.deployment.stop.is_cancelled() {
            break;
        }
        if let Ok(nodes) = cluster.store.nodes().await
            && let Some(node) = nodes.into_iter().find(|n| {
                n.id == current.id
                    && n.boot_id == input.expected_successor_boot_id
                    && n.online
                    && n.url.trim_end_matches('/') == input.successor_url.trim_end_matches('/')
            })
            && cluster.successor_ready(&node).await
        {
            *guard = Destination::Remote(Box::new(node));
            state.deployment.phase.store(3, Ordering::Release);
            keepalive_stop.cancel();
            let _ = keepalive.await;
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    keepalive_stop.cancel();
    let _ = keepalive.await;
    Err(Error::cancelled())
}
