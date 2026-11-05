//! One upstream socket per authenticated binding/thread, with conservative
//! connection-local continuation. Conversation contents are never retained.
use crate::client_pool::{HttpTransport, transport_error};
use crate::responses_ws_incremental::Continuation;
use bytes::Bytes;
use futures::{SinkExt, StreamExt, stream};
use http::{HeaderMap, HeaderValue, StatusCode, Version};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    future::Future,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};
use tokio::{
    sync::{OwnedSemaphorePermit, Semaphore},
    time::{Duration, Instant},
};
use tokio_tungstenite::{
    WebSocketStream,
    tungstenite::{
        Message,
        handshake::{client::generate_key, derive_accept_key},
        protocol::{Role, WebSocketConfig},
    },
};
use tokio_util::sync::CancellationToken;
use xxgate_core::{
    Error, Result,
    protocol::{PreparedRequest, ResponseConnectionKey, UpstreamResponse},
    settings::LiveSettings,
};

const MAX_CONNECTIONS: usize = 256;
const IDLE_TTL: Duration = Duration::from_secs(90);
const MAX_AGE: Duration = Duration::from_secs(55 * 60);
type Socket = WebSocketStream<reqwest::Upgraded>;

#[derive(Clone, PartialEq, Eq)]
struct Signature {
    account: Option<uuid::Uuid>,
    version: i64,
    url: String,
    tls: String,
    headers: [u8; 32],
}

impl Signature {
    fn new(request: &PreparedRequest) -> Self {
        // Per-turn metadata travels in each create event. All other handshake
        // headers must match, including credentials, identity and client profile.
        let mut headers: Vec<_> = request
            .headers
            .iter()
            .filter(|(name, _)| name.as_str() != "x-codex-turn-metadata")
            .map(|(name, value)| (name.as_str(), value.as_bytes()))
            .collect();
        headers.sort_unstable();
        let mut digest = Sha256::new();
        for (name, value) in headers {
            digest.update(name.as_bytes());
            digest.update([0]);
            digest.update(value);
            digest.update([0]);
        }
        Self {
            account: request.account_id,
            version: request.profile_version,
            url: request.url.clone(),
            tls: request.tls_backend.clone(),
            headers: digest.finalize().into(),
        }
    }
}

struct Connection {
    socket: Socket,
    continuation: Continuation,
    last_response_id: Option<String>,
    signature: Signature,
    created: Instant,
    idle_since: Instant,
    _permit: OwnedSemaphorePermit,
}

impl Connection {
    fn fresh(&self) -> bool {
        self.created.elapsed() < MAX_AGE && self.idle_since.elapsed() < IDLE_TTL
    }
}

struct PoolInner {
    idle: Mutex<HashMap<ResponseConnectionKey, Connection>>,
    slots: Arc<Semaphore>,
    sweeping: AtomicBool,
}

#[derive(Clone)]
pub(crate) struct ConnectionPool(Arc<PoolInner>);

impl Default for ConnectionPool {
    fn default() -> Self {
        Self(Arc::new(PoolInner {
            idle: Mutex::new(HashMap::new()),
            slots: Arc::new(Semaphore::new(MAX_CONNECTIONS)),
            sweeping: AtomicBool::new(false),
        }))
    }
}

impl ConnectionPool {
    fn start_sweeper(&self) {
        if self.0.sweeping.swap(true, Ordering::Relaxed) {
            return;
        }
        let weak = Arc::downgrade(&self.0);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(15)).await;
                let Some(pool) = weak.upgrade() else {
                    break;
                };
                pool.idle
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .retain(|_, c| c.fresh());
            }
        });
    }

    fn take(&self, key: &ResponseConnectionKey, signature: &Signature) -> Option<Connection> {
        let mut idle = self.0.idle.lock().unwrap_or_else(|e| e.into_inner());
        idle.retain(|k, c| {
            c.fresh()
                && (k.thread.session != key.thread.session || k.binding_id == key.binding_id)
                && (c.signature.account != signature.account
                    || c.signature.version == signature.version)
        });
        idle.remove(key).filter(|c| &c.signature == signature)
    }

    fn reserve(&self) -> Option<OwnedSemaphorePermit> {
        if let Ok(permit) = self.0.slots.clone().try_acquire_owned() {
            return Some(permit);
        }
        // Prefer an active thread over an idle socket, without exceeding the cap.
        let mut idle = self.0.idle.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(key) = idle
            .iter()
            .min_by_key(|(_, c)| c.idle_since)
            .map(|(k, _)| k.clone())
        {
            idle.remove(&key);
        }
        self.0.slots.clone().try_acquire_owned().ok()
    }

    fn put(&self, key: ResponseConnectionKey, mut connection: Connection) {
        if connection.created.elapsed() >= MAX_AGE {
            return;
        }
        connection.idle_since = Instant::now();
        self.0
            .idle
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key, connection);
    }

    pub(crate) fn close_session(&self, session: uuid::Uuid) {
        self.0
            .idle
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|key, _| key.websocket_session != Some(session));
    }
}

fn ws_error() -> Error {
    // Do not log tungstenite/reqwest errors: they can contain auth or input data.
    Error::new(
        502,
        "websocket_transport_failed",
        "Upstream WebSocket closed or failed; request was not replayed",
    )
}

async fn connecting<T>(
    settings: &LiveSettings,
    cancel: &CancellationToken,
    future: impl Future<Output = Result<T>>,
) -> Result<T> {
    let mut settings = settings.subscribe();
    let started = Instant::now();
    tokio::pin!(future);
    loop {
        let timeout = settings.borrow_and_update().connect_timeout_ms;
        tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(Error::cancelled()),
            _ = settings.changed() => {},
            _ = tokio::time::sleep_until(started + Duration::from_millis(timeout)) => return Err(Error::new(504, "connect_timeout", "Upstream WebSocket handshake timed out")),
            result = &mut future => return result,
        }
    }
}

/// Check an idle socket before sending any generation request. A stale socket
/// can be replaced here without replaying work or risking duplicate billing.
async fn probe(socket: &mut Socket) -> Result<()> {
    let nonce = generate_key();
    socket
        .send(Message::Ping(Bytes::copy_from_slice(nonce.as_bytes())))
        .await
        .map_err(|_| ws_error())?;
    loop {
        match socket.next().await {
            Some(Ok(Message::Pong(bytes))) if bytes.as_ref() == nonce.as_bytes() => return Ok(()),
            Some(Ok(Message::Ping(_))) => socket.flush().await.map_err(|_| ws_error())?,
            Some(Ok(Message::Pong(_))) => {}
            // Never deliver an old response/event to the next caller.
            _ => return Err(ws_error()),
        }
    }
}

fn create_payload(request: &PreparedRequest) -> Result<Value> {
    let mut value: Value = serde_json::from_slice(&request.body)
        .map_err(|_| Error::invalid("Invalid Responses payload"))?;
    let body = value
        .as_object_mut()
        .ok_or_else(|| Error::invalid("Invalid Responses payload"))?;
    body.remove("stream");
    body.remove("stream_options");
    body.remove("background");
    body.insert("type".into(), json!("response.create"));
    if let Some(turn) = request.headers.get("x-codex-turn-metadata") {
        let metadata = body.entry("client_metadata").or_insert_with(|| json!({}));
        metadata
            .as_object_mut()
            .ok_or_else(|| Error::invalid("Invalid client metadata"))?
            .insert(
                "x-codex-turn-metadata".into(),
                json!(
                    turn.to_str()
                        .map_err(|_| Error::invalid("Invalid turn metadata"))?
                ),
            );
    }
    Ok(value)
}

impl HttpTransport {
    pub(crate) async fn send_websocket_response(
        &self,
        request: PreparedRequest,
        key: ResponseConnectionKey,
        cancel: CancellationToken,
    ) -> Result<UpstreamResponse> {
        let client = self.client(&request)?; // Preserve TLS, proxy, URL validation and cookies.
        let mut payload = create_payload(&request)?;
        let signature = Signature::new(&request);
        self.responses_ws.start_sweeper();
        let mut connection = self.responses_ws.take(&key, &signature);
        if let Some(c) = &mut connection
            && connecting(&self.settings, &cancel, probe(&mut c.socket))
                .await
                .is_err()
        {
            connection = None;
        }
        if cancel.is_cancelled() {
            return Err(Error::cancelled());
        }
        let parent = payload.get("previous_response_id").and_then(Value::as_str);
        if let Some(parent) = parent
            && (key.websocket_session.is_none()
                || connection
                    .as_ref()
                    .and_then(|c| c.last_response_id.as_deref())
                    != Some(parent))
        {
            return Err(Error::new(
                400,
                "previous_response_not_found",
                "The previous response is unavailable on this connection; resend full input without previous_response_id",
            ));
        }
        let reused = connection.is_some();
        let mut response_headers = HeaderMap::new();
        let mut connection = match connection {
            Some(c) => c,
            None => {
                let Some(permit) = self.responses_ws.reserve() else {
                    return Err(Error::new(
                        503,
                        "websocket_capacity_exhausted",
                        "Upstream WebSocket capacity is exhausted; retry later",
                    ));
                };
                let nonce = generate_key();
                let mut headers = request.headers.clone();
                headers.remove("content-type");
                headers.remove("content-length");
                headers.remove("accept");
                headers.insert("connection", HeaderValue::from_static("Upgrade"));
                headers.insert("upgrade", HeaderValue::from_static("websocket"));
                headers.insert("sec-websocket-version", HeaderValue::from_static("13"));
                headers.insert(
                    "sec-websocket-key",
                    HeaderValue::from_str(&nonce).map_err(|_| ws_error())?,
                );
                headers.insert(
                    "openai-beta",
                    HeaderValue::from_static("responses_websockets=2026-02-06"),
                );
                let response = connecting(&self.settings, &cancel, async {
                    client
                        .get(&request.url)
                        .version(Version::HTTP_11)
                        .headers(headers)
                        .send()
                        .await
                        .map_err(transport_error)
                })
                .await?;
                if response.status() != StatusCode::SWITCHING_PROTOCOLS {
                    if response.status().is_success() || response.status().is_redirection() {
                        return Err(Error::new(
                            502,
                            "invalid_websocket_handshake",
                            "Upstream did not upgrade to WebSocket",
                        ));
                    }
                    // Preserve HTTP authentication/quota/error handling.
                    return Ok(UpstreamResponse {
                        status: response.status().as_u16(),
                        headers: response.headers().clone(),
                        bytes: Box::pin(
                            response
                                .bytes_stream()
                                .map(|r| r.map_err(transport_error))
                                .take_until(cancel.cancelled_owned()),
                        ),
                    });
                }
                let expected = derive_accept_key(nonce.as_bytes());
                let h = response.headers();
                if h.get("sec-websocket-accept").and_then(|v| v.to_str().ok())
                    != Some(expected.as_str())
                    || !h
                        .get("upgrade")
                        .and_then(|v| v.to_str().ok())
                        .is_some_and(|v| v.eq_ignore_ascii_case("websocket"))
                    || !h
                        .get("connection")
                        .and_then(|v| v.to_str().ok())
                        .is_some_and(|v| {
                            v.split(',')
                                .any(|part| part.trim().eq_ignore_ascii_case("upgrade"))
                        })
                    || h.contains_key("sec-websocket-extensions")
                    || h.contains_key("sec-websocket-protocol")
                {
                    return Err(Error::new(
                        502,
                        "invalid_websocket_handshake",
                        "Invalid upstream WebSocket upgrade",
                    ));
                }
                response_headers = h.clone();
                let upgraded = connecting(&self.settings, &cancel, async {
                    response.upgrade().await.map_err(transport_error)
                })
                .await?;
                let limit = self.settings.current().sse_event_limit_bytes;
                let config = WebSocketConfig::default()
                    .max_message_size(Some(limit))
                    .max_frame_size(Some(limit));
                let socket =
                    WebSocketStream::from_raw_socket(upgraded, Role::Client, Some(config)).await;
                Connection {
                    socket,
                    continuation: Continuation::default(),
                    last_response_id: None,
                    signature,
                    created: Instant::now(),
                    idle_since: Instant::now(),
                    _permit: permit,
                }
            }
        };
        connection.last_response_id = None;
        let plan = connection.continuation.prepare(
            &mut payload,
            key.websocket_session.is_none()
                && self
                    .settings
                    .current()
                    .responses_websocket_incremental_enabled,
        )?;
        let payload = serde_json::to_string(&payload)
            .map_err(|_| Error::invalid("Invalid Responses payload"))?;
        let payload_bytes = payload.len();
        // From this point onwards, every error is returned without fallback/retry.
        tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(Error::cancelled()),
            result = connection.socket.send(Message::Text(payload.into())) => result.map_err(|_| ws_error())?,
        }
        tracing::debug!(
            reused,
            incremental = plan.skipped_items > 0,
            input_items = plan.input_items,
            sent_items = plan.input_items - plan.skipped_items,
            payload_bytes,
            "upstream Responses WebSocket dispatched"
        );
        response_headers.insert(
            "content-type",
            HeaderValue::from_static("text/event-stream"),
        );
        // Reused connections intentionally have no old handshake quotas/request ID.
        let pool = self.responses_ws.clone();
        let settings = self.settings.clone();
        let bytes = stream::unfold(Some(connection), move |state| {
            let pool = pool.clone();
            let key = key.clone();
            let cancel = cancel.clone();
            let settings = settings.clone();
            async move {
                let mut connection = state?;
                let result = tokio::select! {
                    biased;
                    _ = cancel.cancelled() => Err(Error::cancelled()),
                    result = next_event(&mut connection, &settings) => result,
                };
                match result {
                    Ok((bytes, terminal, reusable)) => {
                        if terminal {
                            if reusable && !cancel.is_cancelled() && key.reusable {
                                pool.put(key, connection);
                            }
                            Some((Ok(bytes), None))
                        } else {
                            Some((Ok(bytes), Some(connection)))
                        }
                    }
                    Err(error) => Some((Err(error), None)),
                }
            }
        });
        Ok(UpstreamResponse {
            status: 200,
            headers: response_headers,
            bytes: Box::pin(bytes),
        })
    }
}

async fn next_event(
    connection: &mut Connection,
    settings: &LiveSettings,
) -> Result<(Bytes, bool, bool)> {
    loop {
        return match connection.socket.next().await {
            Some(Ok(Message::Text(text))) => {
                if text.len() > settings.current().sse_event_limit_bytes {
                    return Err(Error::new(
                        502,
                        "event_too_large",
                        "Upstream WebSocket event exceeds the configured limit",
                    ));
                }
                let value: Value = serde_json::from_str(&text).map_err(|_| {
                    Error::new(
                        502,
                        "invalid_upstream_event",
                        "Upstream WebSocket event is not valid JSON",
                    )
                })?;
                let kind = value.get("type").and_then(Value::as_str).ok_or_else(|| {
                    Error::new(
                        502,
                        "invalid_upstream_event",
                        "Upstream WebSocket event has no type",
                    )
                })?;
                let reusable = matches!(kind, "response.completed" | "response.incomplete");
                let terminal = reusable || matches!(kind, "response.failed" | "error");
                connection.last_response_id = if kind == "response.completed" {
                    value
                        .pointer("/response/id")
                        .and_then(Value::as_str)
                        .filter(|id| !id.is_empty() && id.len() <= 512)
                        .map(str::to_owned)
                } else {
                    None
                };
                connection.continuation.observe(
                    &value,
                    settings.current().responses_websocket_incremental_enabled,
                )?;
                // Compact JSON first: embedded newlines must not split SSE data.
                Ok((
                    Bytes::from(format!("data: {}\n\n", value)),
                    terminal,
                    reusable,
                ))
            }
            Some(Ok(Message::Ping(_))) => {
                connection.socket.flush().await.map_err(|_| ws_error())?;
                continue;
            }
            Some(Ok(Message::Pong(_))) => continue,
            _ => Err(ws_error()),
        };
    }
}

#[cfg(test)]
#[path = "responses_ws_tests.rs"]
mod tests;
