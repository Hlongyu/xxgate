//! Account ownership is fixed per worker. Only session placement is coordinated.
use crate::http::AppState;
use axum::{
    body::Body,
    extract::State,
    http::{HeaderMap, HeaderValue, Method},
    response::{IntoResponse, Response},
};
use bytes::Bytes;
use serde_json::{Value, json};
use std::{
    collections::HashSet,
    sync::{
        Arc,
        atomic::{AtomicI64, AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::sync::Mutex;
use uuid::Uuid;
use xxgate_core::{
    Error, Result, access::GatewayKey, identity::SessionKey, protocol::GatewayRequest,
};
use xxgate_postgres::{PgStore, cluster::Node};

pub const TOKEN_HEADER: &str = "x-xxgate-cluster-token";
pub const REQUEST_HEADER: &str = "x-xxgate-request-id";
pub const WS_HEADER: &str = "x-xxgate-websocket-session";

pub struct Cluster {
    pub store: Arc<PgStore>,
    pub router: bool,
    pub boot_id: Uuid,
    token: String,
    client: reqwest::Client,
    revision: AtomicI64,
    reload: Mutex<()>,
    sequence: AtomicU64,
}

impl Cluster {
    pub fn new(
        store: Arc<PgStore>,
        router: bool,
        token: String,
        boot_id: Uuid,
    ) -> Result<Arc<Self>> {
        if token.len() < 32 || HeaderValue::from_str(&token).is_err() {
            return Err(Error::invalid(
                "XXGATE_CLUSTER_TOKEN must contain at least 32 header-safe bytes",
            ));
        }
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(5))
            .pool_idle_timeout(Duration::from_secs(60))
            .build()
            .map_err(|_| Error::storage())?;
        Ok(Arc::new(Self {
            store,
            router,
            boot_id,
            token,
            client,
            revision: AtomicI64::new(0),
            reload: Mutex::new(()),
            sequence: AtomicU64::new(0),
        }))
    }

    pub fn authenticated(&self, headers: &HeaderMap) -> bool {
        // Compare digests to avoid revealing the token through prefix timing.
        headers
            .get(TOKEN_HEADER)
            .and_then(|h| h.to_str().ok())
            .is_some_and(|v| {
                let a = xxgate_core::access::secret_hash(v);
                let b = xxgate_core::access::secret_hash(&self.token);
                a.bytes().zip(b.bytes()).fold(0u8, |d, (a, b)| d | (a ^ b)) == 0
            })
    }

    pub async fn node(&self, id: &str) -> Result<Node> {
        self.store
            .nodes()
            .await?
            .into_iter()
            .find(|n| n.id == id && n.role == "worker" && n.online)
            .ok_or_else(|| {
                Error::new(
                    503,
                    "node_unavailable",
                    "The assigned worker is offline; the request was not replayed on another node",
                )
            })
    }

    pub async fn select(&self, key: &GatewayKey, request: &GatewayRequest) -> Result<Node> {
        let session = SessionKey {
            key_id: key.id,
            client_session_id: request.identity.session_id.clone(),
        };
        if !request.stateless
            && let Some(node) = self.store.session_node(&session).await?
        {
            return self.node(&node).await;
        }
        use xxgate_core::application::ports::SettingsStore;
        let model = self
            .store
            .models()
            .await?
            .into_iter()
            .find(|m| m.enabled && m.id == request.model)
            .ok_or_else(|| {
                Error::new(400, "model_not_found", "The requested model is unavailable")
            })?;
        let mut eligible = HashSet::new();
        for (node_id, a) in self.store.node_accounts().await? {
            if a.enabled
                && a.group_ids.contains(&key.group_id)
                && a.accepts(request.client_origin.source)
                && a.supports(&model.upstream)
                && (request.websocket_session.is_none() || a.responses_websocket_enabled)
            {
                eligible.insert(node_id);
            }
        }
        let mut nodes: Vec<_> = self
            .store
            .nodes()
            .await?
            .into_iter()
            .filter(|n| n.role == "worker" && n.online && n.accepting && eligible.contains(&n.id))
            .collect();
        // Rotate equal-load nodes instead of always choosing the lexicographic first.
        if !nodes.is_empty() {
            let rotate = self.sequence.fetch_add(1, Ordering::Relaxed) as usize % nodes.len();
            nodes.rotate_left(rotate);
        }
        nodes.sort_by(|a, b| load(a).total_cmp(&load(b)));
        let node = nodes.into_iter().next().ok_or_else(|| {
            Error::new(
                503,
                "no_available_node",
                "No online worker has an eligible account",
            )
        })?;
        if request.stateless {
            return Ok(node);
        }
        let winner = self.store.claim_session(&session, &node.id).await?;
        if winner == node.id {
            Ok(node)
        } else {
            self.node(&winner).await
        }
    }

    pub async fn check_owner(&self, key: &GatewayKey, request: &GatewayRequest) -> Result<()> {
        if request.stateless {
            return Ok(());
        }
        let session = SessionKey {
            key_id: key.id,
            client_session_id: request.identity.session_id.clone(),
        };
        if self
            .store
            .claim_session(&session, self.store.node_id())
            .await?
            != self.store.node_id()
        {
            return Err(Error::new(
                409,
                "session_node_mismatch",
                "This session belongs to another worker; use the cluster entry URL",
            ));
        }
        Ok(())
    }

    /// Streams responses with backpressure; never retries a generation or follows redirects.
    pub async fn forward(
        &self,
        node: &Node,
        method: Method,
        path: &str,
        mut headers: HeaderMap,
        body: Body,
        websocket: Option<Uuid>,
    ) -> Result<Response> {
        sanitize(&mut headers);
        headers.remove(WS_HEADER);
        headers.remove("x-xxgate-node");
        let mut token = HeaderValue::from_str(&self.token).map_err(|_| Error::storage())?;
        token.set_sensitive(true);
        headers.insert(TOKEN_HEADER, token);
        if let Some(id) = websocket {
            headers.insert(
                WS_HEADER,
                HeaderValue::from_str(&id.to_string()).map_err(|_| Error::storage())?,
            );
        }
        let response = self
            .client
            .request(method, format!("{}{path}", node.url.trim_end_matches('/')))
            .headers(headers)
            .body(reqwest::Body::wrap_stream(body.into_data_stream()))
            .send()
            .await
            .map_err(|_| {
                Error::new(
                    502,
                    "node_connection_failed",
                    "Worker connection failed; the request was not retried",
                )
            })?;
        let status = response.status();
        let mut headers = response.headers().clone();
        sanitize(&mut headers);
        headers.insert(
            "x-xxgate-node",
            HeaderValue::from_str(&node.id).map_err(|_| Error::storage())?,
        );
        let mut result = Body::from_stream(response.bytes_stream()).into_response();
        *result.status_mut() = status;
        *result.headers_mut() = headers;
        Ok(result)
    }

    pub async fn forward_generation(
        &self,
        node: &Node,
        path: &str,
        mut headers: HeaderMap,
        raw: Bytes,
        websocket: Option<Uuid>,
        request_id: Uuid,
    ) -> Result<Response> {
        headers.insert(
            REQUEST_HEADER,
            HeaderValue::from_str(&request_id.to_string()).map_err(|_| Error::storage())?,
        );
        self.forward(
            node,
            Method::POST,
            path,
            headers,
            Body::from(raw),
            websocket,
        )
        .await
    }

    pub async fn reload(&self, state: &AppState) -> Result<()> {
        use xxgate_core::application::ports::{AccessStore, SettingsStore};
        let _serial = self.reload.lock().await;
        let _mutations = state.gateway.mutations.lock().await;
        let revision = self.store.revision().await?;
        if revision == self.revision.load(Ordering::Acquire) {
            return Ok(());
        }
        let settings = self.store.settings().await?;
        if settings.version > state.gateway.settings.current().version {
            crate::logging::set_level(settings.log_level);
            state.gateway.settings.publish(settings);
        }
        state
            .gateway
            .scheduler
            .replace_keys(self.store.keys().await?);
        state.gateway.scheduler.wake();
        self.revision.store(revision, Ordering::Release);
        Ok(())
    }

    pub async fn publish(&self, state: &AppState) -> Result<()> {
        let stats = state.gateway.scheduler.stats();
        let capacity = state
            .gateway
            .scheduler
            .accounts()
            .iter()
            .filter(|a| a.enabled)
            .map(|a| a.max_inflight as u64)
            .sum::<u64>()
            .min(state.gateway.settings.current().global_max_inflight as u64);
        let memory = state.gateway.memory_admission.status();
        let runtime = json!({"queue":stats,"capacity":capacity,"memory_admission":memory,"config_version":state.gateway.settings.current().version});
        let accepting = !self.router
            && capacity > 0
            && !stats.paused
            && !memory.blocked
            && !state.gateway.shutdown.is_cancelled();
        self.store.heartbeat(self.boot_id, accepting, runtime).await
    }

    pub async fn broadcast_reload(&self) {
        use futures::{StreamExt, stream};
        let Ok(nodes) = self.store.nodes().await else {
            return;
        };
        stream::iter(nodes.into_iter().filter(|n|n.online && n.id!=self.store.node_id()))
            .for_each_concurrent(8,|n| async move {
                if self.client.post(format!("{}/internal/reload",n.url.trim_end_matches('/')))
                    .header(TOKEN_HEADER,&self.token).timeout(Duration::from_secs(3)).send().await.is_err() {
                    tracing::warn!(node_id=%n.id,"configuration push failed; worker will poll revision");
                }
            }).await;
    }

    pub async fn close_websocket(&self, id: Uuid) {
        use futures::{StreamExt, stream};
        let Ok(nodes) = self.store.nodes().await else {
            return;
        };
        stream::iter(nodes.into_iter().filter(|n| n.online && n.role == "worker"))
            .for_each_concurrent(8, |n| async move {
                let _ = self
                    .client
                    .delete(format!(
                        "{}/internal/websocket/{id}",
                        n.url.trim_end_matches('/')
                    ))
                    .header(TOKEN_HEADER, &self.token)
                    .timeout(Duration::from_secs(3))
                    .send()
                    .await;
            })
            .await;
    }
}

fn load(node: &Node) -> f64 {
    let q = &node.runtime["queue"];
    (q["inflight"].as_u64().unwrap_or(0) + q["queued"].as_u64().unwrap_or(0)) as f64
        / node.runtime["capacity"].as_u64().unwrap_or(1).max(1) as f64
}

fn sanitize(headers: &mut HeaderMap) {
    if let Some(connection) = headers
        .get("connection")
        .and_then(|h| h.to_str().ok())
        .map(str::to_owned)
    {
        for name in connection.split(',') {
            headers.remove(name.trim());
        }
    }
    for name in [
        "connection",
        "keep-alive",
        "proxy-authenticate",
        "proxy-authorization",
        "te",
        "trailer",
        "transfer-encoding",
        "upgrade",
        "host",
        TOKEN_HEADER,
    ] {
        headers.remove(name);
    }
}

pub async fn nodes(State(state): State<AppState>) -> crate::http::ApiResult<axum::Json<Value>> {
    let items = if let Some(c) = &state.cluster {
        c.store.nodes().await?
    } else {
        vec![]
    };
    Ok(axum::Json(
        json!({"enabled":state.cluster.is_some(),"router":state.cluster.as_ref().is_some_and(|c|c.router),"items":items}),
    ))
}

/// A short outage pauses new dispatch while allowing already-issued streams
/// to finish. A different boot ID is a definite ownership loss and is fatal.
#[derive(Default)]
struct CoordinationHealth {
    failed_since: Option<tokio::time::Instant>,
}
impl CoordinationHealth {
    fn observe(&mut self, result: &Result<()>, now: tokio::time::Instant) -> bool {
        match result {
            Ok(()) => {
                self.failed_since = None;
                false
            }
            Err(error) => {
                let since = *self.failed_since.get_or_insert(now);
                error.code == "node_ownership_lost"
                    || now.duration_since(since) >= Duration::from_secs(30)
            }
        }
    }
}

pub fn start(state: AppState) {
    let Some(cluster) = state.cluster.clone() else {
        return;
    };
    state.gateway.tasks.spawn({let state=state.clone();async move {
        let mut health=CoordinationHealth::default();
        let mut tick=tokio::time::interval(Duration::from_secs(1));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! { _=state.gateway.shutdown.cancelled()=>break, _=tick.tick()=>{} }
            let result=tokio::time::timeout(Duration::from_secs(3),async {
                cluster.reload(&state).await?;
                cluster.publish(&state).await
            }).await.unwrap_or_else(|_|Err(Error::new(503,"coordination_timeout","Cluster coordination timed out")));
            state.gateway.scheduler.set_coordination_paused(result.is_err());
            if health.observe(&result,tokio::time::Instant::now()) {
                tracing::error!("cluster coordination lost for 30 seconds or ownership replaced; stopping node");
                state.gateway.shutdown.cancel();break;
            }
            if let Err(error)=result {tracing::warn!(code=%error.code,"cluster coordination unavailable; pausing new dispatch");}
        }
        let _=tokio::time::timeout(Duration::from_secs(3),cluster.store.heartbeat(cluster.boot_id,false,json!({"stopped":true}))).await;
    }});
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn brief_outages_recover_but_expired_grace_and_ownership_loss_stop() {
        let now = tokio::time::Instant::now();
        let mut health = CoordinationHealth::default();
        assert!(!health.observe(&Err(Error::storage()), now));
        assert!(!health.observe(&Err(Error::storage()), now + Duration::from_secs(29)));
        assert!(!health.observe(&Ok(()), now + Duration::from_secs(29)));
        assert!(!health.observe(&Err(Error::storage()), now + Duration::from_secs(30)));
        assert!(health.observe(&Err(Error::storage()), now + Duration::from_secs(60)));
        let mut fresh = CoordinationHealth::default();
        assert!(fresh.observe(&Err(Error::new(503, "node_ownership_lost", "lost")), now));
    }
}
