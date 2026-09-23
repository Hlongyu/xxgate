use super::{ApiResult, AppState, auth::gateway_key};
use axum::{
    Json,
    body::Body,
    extract::{Request, State},
    http::{HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
};
use bytes::BytesMut;
use chrono::Utc;
use futures::StreamExt;
use http_body_util::BodyExt;
use serde_json::{Value, json};
use std::convert::Infallible;
use tokio_stream::wrappers::ReceiverStream;
use uuid::Uuid;
use xxgate_core::Error;
use xxgate_core::protocol::RequestKind;
use xxgate_core::{audit::RequestRecord, pricing::Valuation, usage::Usage};

pub async fn models(State(state): State<AppState>, headers: HeaderMap) -> ApiResult<Json<Value>> {
    let key = gateway_key(&state, &headers).await?;
    let source = xxgate_codex::client_source::detect(&headers, &Value::Null).source;
    let models = state
        .gateway
        .store
        .models()
        .await?
        .into_iter()
        .filter(|m| {
            m.enabled
                && state
                    .gateway
                    .scheduler
                    .supports_group_model(key.group_id, &m.upstream, source)
        })
        .collect::<Vec<_>>();
    let capabilities = super::public_models::catalog(&state, key.group_id, source, &models).await?;
    let data: Vec<_> = models
        .iter()
        .map(|m| json!({"id":m.id,"object":"model","owned_by":"xxgate"}))
        .collect();
    Ok(Json(
        json!({"object":"list","data":data,"models":capabilities}),
    ))
}
pub async fn responses(State(state): State<AppState>, request: Request) -> Response {
    handle(state, request, RequestKind::Responses).await
}
pub async fn compact(State(state): State<AppState>, request: Request) -> Response {
    handle(state, request, RequestKind::Compact).await
}
pub async fn search(State(state): State<AppState>, request: Request) -> Response {
    handle(state, request, RequestKind::Search).await
}
async fn handle(state: AppState, request: Request, kind: RequestKind) -> Response {
    let id = Uuid::new_v4();
    let started = tokio::time::Instant::now();
    let created_at = Utc::now();
    let mut context = RejectedContext::default();
    let result = responses_inner(&state, request, id, &mut context, kind).await;
    let mut response = match result {
        Ok(response) => response,
        Err(error) => {
            let version = state.gateway.settings.current().version;
            if error.0.code == "session_safety_blocked" {
                context.stage = "safety_policy";
            }
            context.diagnostics["failure_stage"] = json!(context.stage);
            context.diagnostics["body_bytes"] = json!(context.bytes);
            context.diagnostics["error"] =
                json!({"code":error.0.code,"message":error.0.message,"status":error.0.status});
            let record = RequestRecord {
                client_turn_state: context.client_turn_state,
                client_origin: serde_json::from_value(context.diagnostics["client_origin"].clone())
                    .ok(),
                stateless: context.stateless,
                ingress_diagnostics: Some(context.diagnostics),
                kind,
                compaction: context.compaction,
                stream: if kind != RequestKind::Responses {
                    Some(false)
                } else {
                    context.stream
                },
                search_price: None,
                id,
                key_id: context.key_id.unwrap_or_else(Uuid::nil),
                group_id: context.group_id,
                client_session_id: context.session,
                client_thread_id: context.thread,
                model: context.model,
                provider: "openai".into(),
                upstream_model: None,
                response_model: None,
                requested_tier: None,
                reasoning_effort: None,
                account_id: None,
                binding_id: None,
                binding_generation: None,
                state: "rejected".into(),
                created_at,
                finished_at: Some(Utc::now()),
                queue_ms: None,
                first_event_ms: None,
                first_content_ms: None,
                total_ms: Some(started.elapsed().as_millis() as u64),
                upstream_status: None,
                upstream_headers_ms: None,
                upstream_request_id: None,
                error_code: Some(error.0.code.clone()),
                error_message: Some(error.0.message.clone()),
                upstream_error: error.0.upstream.clone(),
                usage: Usage {
                    complete: true,
                    source: "not_executed".into(),
                    ..Usage::default()
                },
                valuation: Some(Valuation {
                    status: "not_executed".into(),
                    price_version: None,
                    cny: Some(rust_decimal::Decimal::ZERO),
                    items: vec![],
                }),
                config_version: version,
                config_versions: vec![version],
                body_bytes: context.bytes,
                upstream_attempts: 0,
            };
            let saved = async {
                state.gateway.store.begin_request(&record).await?;
                state.gateway.store.finish_request(&record).await
            }
            .await;
            if let Err(save_error) = saved {
                tracing::error!(request_id=%id,code=%save_error.code,"rejected request persistence failed");
            }
            tracing::warn!(request_id=%id,key_id=%record.key_id,group_id=?record.group_id,model=%record.model,code=%error.0.code,status=error.0.status,diagnostics=%record.ingress_diagnostics.as_ref().unwrap_or(&serde_json::Value::Null),"request rejected");
            error.into_response()
        }
    };
    if let Ok(value) = HeaderValue::from_str(&id.to_string()) {
        response.headers_mut().insert("x-request-id", value);
    }
    response
}

#[derive(Default)]
struct RejectedContext {
    client_turn_state: Option<xxgate_core::turn_state::TurnStateHeader>,
    compaction: Option<xxgate_core::protocol::Compaction>,
    stateless: bool,
    diagnostics: Value,
    stage: &'static str,
    stream: Option<bool>,
    key_id: Option<Uuid>,
    group_id: Option<Uuid>,
    session: String,
    thread: String,
    model: String,
    bytes: usize,
}
async fn responses_inner(
    state: &AppState,
    request: Request,
    id: Uuid,
    context: &mut RejectedContext,
    kind: RequestKind,
) -> ApiResult<Response> {
    if kind == RequestKind::Compact {
        context.compaction = Some(xxgate_core::protocol::Compaction {
            method: xxgate_core::protocol::CompactionMethod::Compact,
            output_observed: None,
        });
    }
    let (parts, mut body) = request.into_parts();
    context.client_turn_state = Some(xxgate_core::turn_state::TurnStateHeader::capture(
        &parts.headers,
    ));
    context.diagnostics = xxgate_codex::identity_input::diagnostics(
        parts.method.as_str(),
        parts.uri.path(),
        &parts.headers,
        None,
        kind,
    );
    context.stage = "authentication";
    let key = gateway_key(state, &parts.headers).await?;
    context.key_id = Some(key.id);
    context.group_id = Some(key.group_id);
    context.stage = "body_encoding";
    if parts.headers.get("content-encoding").is_some() {
        return Err(Error::new(
            415,
            "content_encoding_unsupported",
            "Send an uncompressed JSON request",
        )
        .into());
    }
    let mut memory = state.gateway.memory.lease();
    let mut raw = BytesMut::new();
    context.stage = "body_read";
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        while let Some(frame) = body.frame().await {
            let frame = frame
                .map_err(|_| Error::new(400, "body_read_failed", "Unable to read request body"))?;
            if let Ok(data) = frame.into_data() {
                let size = raw.len().saturating_add(data.len());
                context.bytes = size;
                if size > state.gateway.settings.current().request_body_limit_bytes {
                    return Err(Error::new(
                        413,
                        "request_too_large",
                        "The request exceeds the configured size limit",
                    ));
                }
                // Conservative reservation includes the parsed JSON tree and outgoing copies.
                memory.resize(size.saturating_mul(16))?;
                raw.extend_from_slice(&data);
            }
        }
        Ok::<(), Error>(())
    })
    .await
    .map_err(|_| Error::new(408, "body_timeout", "Request body timed out"))??;
    let length = raw.len();
    context.stage = "json_parse";
    let document: Value = serde_json::from_slice(&raw).map_err(|_| {
        context.diagnostics["body_status"] = json!("invalid_json");
        Error::invalid("The body must contain valid JSON")
    })?;
    context.diagnostics = xxgate_codex::identity_input::diagnostics(
        parts.method.as_str(),
        parts.uri.path(),
        &parts.headers,
        Some(&document),
        kind,
    );
    context.diagnostics["body_bytes"] = json!(length);
    if kind == RequestKind::Responses {
        context.compaction = xxgate_codex::compaction::inspect(&parts.headers, &document)
            .ok()
            .flatten();
    }
    context.stream = if kind != RequestKind::Responses {
        Some(false)
    } else if document.is_object() {
        document.get("stream").map_or(Some(false), Value::as_bool)
    } else {
        None
    };
    context.model = document
        .get("model")
        .and_then(Value::as_str)
        .filter(|s| s.len() <= 160 && !s.chars().any(char::is_control))
        .unwrap_or("")
        .to_owned();
    drop(raw);
    context.stage = "ingress_validation";
    let mut parsed = match kind {
        RequestKind::Search => {
            xxgate_codex::search::parse(&parts.headers, document, parts.uri.query())?
        }
        RequestKind::Compact => xxgate_codex::compaction::parse(&parts.headers, document)?,
        RequestKind::Responses => state.gateway.ingress.parse(&parts.headers, document)?,
    };
    context.stateless = parsed.stateless;
    if !parsed.stateless {
        context.session = parsed.identity.session_id.clone();
        context.thread = parsed.identity.thread_id.clone();
    }
    parsed.ingress_diagnostics = Some(context.diagnostics.clone());
    context.stage = "admission";
    let handle = state.gateway.start(id, key, parsed, length, memory).await?;
    let id = handle.id.to_string();
    let mut response = if handle.stream {
        let stream = ReceiverStream::new(handle.bytes).map(Ok::<_, Infallible>);
        let mut response = Body::from_stream(stream).into_response();
        response.headers_mut().insert(
            "content-type",
            HeaderValue::from_static("text/event-stream; charset=utf-8"),
        );
        response
            .headers_mut()
            .insert("x-accel-buffering", HeaderValue::from_static("no"));
        response
    } else {
        // Keep the receiver alive so cancellation means the caller actually disconnected.
        let _receiver = handle.bytes;
        let result = handle.completion.await.map_err(|_| {
            Error::new(
                503,
                "execution_lost",
                "Request execution ended unexpectedly",
            )
        })?;
        let status = StatusCode::from_u16(result.status).unwrap_or(StatusCode::BAD_GATEWAY);
        let mut response = if let Some(bytes) = result.raw_body {
            (
                status,
                [("content-type", "application/json")],
                Body::from(bytes),
            )
                .into_response()
        } else {
            (status, Json(result.body)).into_response()
        };
        response.headers_mut().extend(result.headers);
        response
    };
    response.headers_mut().insert(
        "x-request-id",
        HeaderValue::from_str(&id).map_err(|_| Error::storage())?,
    );
    Ok(response)
}
