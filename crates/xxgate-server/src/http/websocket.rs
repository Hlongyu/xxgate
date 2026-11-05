//! Native Responses WS ingress. Each message uses the existing authenticated
//! request pipeline; only the transport envelope is adapted here.
use super::{ApiResult, AppState, auth::gateway_key, public};
use axum::{
    body::Body,
    extract::{
        Request, State, WebSocketUpgrade,
        ws::{Message, WebSocket},
    },
    http::HeaderMap,
    response::Response,
};
use http_body_util::BodyExt;
use serde_json::json;
use std::{sync::Arc, time::Duration};
use uuid::Uuid;
use xxgate_core::{Error, protocol::UpstreamTransport};

#[derive(Clone, Copy)]
pub(super) struct Session(pub Uuid);

struct SessionGuard {
    id: Uuid,
    transport: Arc<dyn UpstreamTransport>,
    cluster: Option<Arc<crate::cluster::Cluster>>,
}
impl Drop for SessionGuard {
    fn drop(&mut self) {
        self.transport.close_response_session(self.id);
        if let Some(cluster) = self.cluster.clone().filter(|c| c.router) {
            let id = self.id;
            tokio::spawn(async move {
                cluster.close_websocket(id).await;
            });
        }
    }
}

pub async fn responses(
    State(state): State<AppState>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> ApiResult<Response> {
    gateway_key(&state, &headers).await?;
    state
        .gateway
        .memory_admission
        .check(true)
        .map_err(|r| r.error)?;
    let permit = state
        .websocket_slots
        .clone()
        .try_acquire_owned()
        .map_err(|_| {
            Error::new(
                503,
                "websocket_capacity_exhausted",
                "Gateway WebSocket capacity is exhausted",
            )
        })?;
    let limit = state.gateway.settings.current().request_body_limit_bytes;
    Ok(ws
        .max_message_size(limit)
        .max_frame_size(limit)
        .on_upgrade(move |socket| async move {
            let _permit = permit;
            let session = SessionGuard {
                id: Uuid::new_v4(),
                transport: state.gateway.transport.clone(),
                cluster: state.cluster.clone(),
            };
            let shutdown = state.gateway.shutdown.clone();
            tokio::select! {
                _ = shutdown.cancelled() => {},
                _ = run(socket, state, headers, session.id) => {},
            }
        }))
}

async fn send(socket: &mut WebSocket, message: Message) -> Result<(), ()> {
    tokio::time::timeout(Duration::from_secs(10), socket.send(message))
        .await
        .map_err(|_| ())?
        .map_err(|_| ())
}

async fn busy_message(
    socket: &mut WebSocket,
    message: Option<Result<Message, axum::Error>>,
) -> Result<(), ()> {
    match message {
        Some(Ok(Message::Ping(data))) => send(socket, Message::Pong(data)).await,
        Some(Ok(Message::Pong(_))) => Ok(()),
        Some(Ok(Message::Text(_) | Message::Binary(_))) => {
            let event = json!({"type":"error","status":400,"error":{"type":"invalid_request_error","code":"websocket_request_in_progress","message":"Wait for the current response to finish before sending another request"}});
            let _ = send(socket, Message::Text(event.to_string().into())).await;
            Err(())
        }
        _ => Err(()),
    }
}

async fn run(
    mut socket: WebSocket,
    state: AppState,
    headers: HeaderMap,
    id: Uuid,
) -> Result<(), ()> {
    loop {
        state
            .gateway
            .memory_admission
            .check(false)
            .map_err(|_| ())?;
        let idle = Duration::from_millis(state.gateway.settings.current().sse_idle_timeout_ms);
        let message = tokio::time::timeout(idle, socket.recv())
            .await
            .map_err(|_| ())?;
        let text = match message {
            Some(Ok(Message::Text(text))) => text,
            Some(Ok(Message::Ping(data))) => {
                send(&mut socket, Message::Pong(data)).await?;
                continue;
            }
            Some(Ok(Message::Pong(_))) => continue,
            _ => return Ok(()),
        };
        let mut request = Request::new(Body::from(text.to_string()));
        *request.method_mut() = http::Method::POST;
        *request.uri_mut() = http::Uri::from_static("/v1/responses");
        *request.headers_mut() = headers.clone();
        request.extensions_mut().insert(Session(id));
        // Re-authentication, memory admission, validation, scheduling and audit
        // apply to every message, including after key revocation/group changes.
        let serving = public::responses(State(state.clone()), request);
        tokio::pin!(serving);
        let response = loop {
            tokio::select! {
                biased;
                response = &mut serving => break response,
                message = socket.recv() => busy_message(&mut socket, message).await?,
            }
        };
        if !response.status().is_success() {
            let status = response.status().as_u16();
            let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
                .await
                .map_err(|_| ())?;
            let mut error: serde_json::Value = serde_json::from_slice(&bytes).map_err(|_| ())?;
            error["type"] = json!("error");
            error["status"] = json!(status);
            send(&mut socket, Message::Text(error.to_string().into())).await?;
            continue;
        }
        let mut body = response.into_body();
        let mut events = GatewayEvents::default();
        loop {
            tokio::select! {
                biased;
                frame = body.frame() => {
                    let Some(frame) = frame else {
                        if events.finish().is_err() {
                            let event=json!({"type":"error","status":502,"error":{"code":"worker_stream_interrupted","message":"Worker stream ended before a terminal event; the request was not replayed"}});
                            let _=send(&mut socket,Message::Text(event.to_string().into())).await;
                            return Err(());
                        }
                        break;
                    };
                    let frame = frame.map_err(|_| ())?;
                    if let Ok(data) = frame.into_data() {
                        let limit=state.gateway.settings.current().sse_event_limit_bytes;
                        for event in events.push(&data,limit)? {
                            send(&mut socket,Message::Text(event.into())).await?;
                        }
                        if events.terminal { break; }
                    }
                }
                message = socket.recv() => busy_message(&mut socket, message).await?,
            }
        }
        // Dropping a response receiver cancels unfinished gateway execution.
    }
}

/// HTTP chunk boundaries are unrelated to SSE event or UTF-8 boundaries.
#[derive(Default)]
struct GatewayEvents {
    decoder: xxgate_codex::wire::SseDecoder,
    terminal: bool,
}
impl GatewayEvents {
    fn push(&mut self, bytes: &[u8], limit: usize) -> Result<Vec<String>, ()> {
        let mut output = Vec::new();
        for data in self.decoder.push(bytes, limit).map_err(|_| ())? {
            if self.terminal {
                return Err(());
            }
            #[derive(serde::Deserialize)]
            struct Event<'a> {
                #[serde(rename = "type", borrow)]
                kind: &'a str,
            }
            let event: Event<'_> = serde_json::from_str(&data).map_err(|_| ())?;
            self.terminal = matches!(
                event.kind,
                "response.completed" | "response.incomplete" | "response.failed" | "error"
            );
            output.push(data);
        }
        Ok(output)
    }
    fn finish(&self) -> Result<(), ()> {
        if self.terminal && self.decoder.buffered_bytes() == 0 {
            Ok(())
        } else {
            Err(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn network_fragments_coalescing_heartbeats_and_utf8_preserve_events() {
        let input=concat!(": heartbeat\r\n\r\n", "event: response.output_text.delta\r\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"你好\"}\r\n\r\n", ": heartbeat\n\nevent: response.completed\ndata: {\"type\":\"response.completed\"}\n\n").as_bytes();
        for width in 1..=input.len() {
            let mut decoder = GatewayEvents::default();
            let mut events = vec![];
            for chunk in input.chunks(width) {
                events.extend(decoder.push(chunk, 4096).unwrap());
            }
            assert_eq!(events.len(), 2);
            assert_eq!(
                serde_json::from_str::<serde_json::Value>(&events[0]).unwrap()["delta"],
                "你好"
            );
            assert!(decoder.terminal);
            assert!(decoder.finish().is_ok());
        }
    }
    #[test]
    fn truncated_oversized_and_nonterminal_streams_fail() {
        let mut decoder = GatewayEvents::default();
        assert!(
            decoder
                .push(b"data: {\"type\":\"response.created\"}\n\n", 128)
                .is_ok()
        );
        assert!(decoder.finish().is_err());
        assert!(
            decoder
                .push(b"data: {\"type\":\"response.completed\"}", 128)
                .is_ok()
        );
        assert!(decoder.finish().is_err());
        assert!(GatewayEvents::default().push(&[b'x'; 129], 128).is_err());
        for kind in ["response.failed", "response.incomplete", "error"] {
            let mut decoder = GatewayEvents::default();
            decoder
                .push(format!("data: {{\"type\":\"{kind}\"}}\n\n").as_bytes(), 128)
                .unwrap();
            assert!(decoder.finish().is_ok());
        }
    }
}
