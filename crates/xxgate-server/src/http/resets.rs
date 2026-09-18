use super::{ApiResult, AppState};
use axum::{
    Json,
    extract::{Path, Query, State},
};
use chrono::Utc;
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;
use xxgate_core::Error;

pub(super) async fn view(s: &AppState, id: Uuid) -> xxgate_core::Result<Value> {
    s.gateway
        .scheduler
        .account(id)
        .ok_or_else(Error::not_found)?;
    let snapshot = s.gateway.store.reset_credits(id).await?;
    let now = Utc::now();
    Ok(json!({
        "usable_count": snapshot.as_ref().map(|c| c.credits.iter().filter(|c| c.available(now)).count()),
        "next_credit": snapshot.as_ref().and_then(|c| c.next(now)),
        "snapshot": snapshot,
        "pending": s.gateway.store.reset_operation(id, None).await?
    }))
}

#[derive(Deserialize)]
pub struct StatusQuery {
    operation_id: Option<Uuid>,
}

pub async fn get(
    State(s): State<AppState>,
    Path(id): Path<Uuid>,
    Query(q): Query<StatusQuery>,
) -> ApiResult<Json<Value>> {
    let mut value = view(&s, id).await?;
    if let Some(operation) = q.operation_id {
        value["operation"] =
            serde_json::to_value(s.gateway.store.reset_operation(id, Some(operation)).await?)
                .map_err(|_| Error::storage())?;
    }
    Ok(Json(value))
}

pub async fn refresh(State(s): State<AppState>, Path(id): Path<Uuid>) -> ApiResult<Json<Value>> {
    s.gateway.collect_reset_credits(id).await?;
    Ok(Json(view(&s, id).await?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Consume {
    operation_id: Uuid,
    expected_credit_id: String,
}

pub async fn consume(
    State(s): State<AppState>,
    Path(id): Path<Uuid>,
    Json(input): Json<Consume>,
) -> ApiResult<Json<Value>> {
    let state = s.clone();
    let task = s.gateway.tasks.spawn(async move {
        let operation = state
            .gateway
            .consume_reset(id, input.operation_id, &input.expected_credit_id)
            .await?;
        // The redemption outcome is durable before these independent reads.
        // A refresh failure must never turn a confirmed reset into a failed one.
        let (credits, quotas) = tokio::join!(
            state.gateway.collect_reset_credits(id),
            state.gateway.collect_quotas(id)
        );
        let refresh_errors: Vec<_> = [credits.err(), quotas.err()]
            .into_iter()
            .flatten()
            .collect();
        Ok::<_, Error>(json!({"operation":operation,"refresh_errors":refresh_errors}))
    });
    Ok(Json(task.await.map_err(|_| {
        Error::new(
            503,
            "reset_interrupted",
            "操作结果待核实，请重新打开重置详情",
        )
    })??))
}
