use super::{ApiResult, AppState};
use axum::{
    Json,
    extract::{Path, State},
};
use chrono::{DateTime, Utc};
use futures::{StreamExt, stream};
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use uuid::Uuid;
use xxgate_core::types::ModelRef;

#[derive(Default, Clone, Serialize)]
pub(crate) struct SyncJob {
    running: bool,
    total: usize,
    completed: usize,
    started_at: Option<DateTime<Utc>>,
    finished_at: Option<DateTime<Utc>>,
    results: Vec<Value>,
}
pub(super) fn schedule(s: &AppState, id: Uuid) {
    let gateway = s.gateway.clone();
    s.gateway.tasks.spawn(async move {
        if let Err(error) = gateway.sync_models(id).await {
            tracing::warn!(account_id=%id,code=%error.code,"automatic model discovery failed");
        }
    });
}
pub async fn sync_account(
    State(s): State<AppState>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<Value>> {
    let models = s.gateway.sync_models(id).await?;
    Ok(Json(json!({"models":models})))
}
pub async fn sync_all(State(s): State<AppState>) -> Json<Value> {
    let mut job = s.model_sync.write().await;
    if job.running {
        return Json(json!({"running":true,"already_running":true}));
    }
    let accounts = s.gateway.scheduler.accounts();
    *job = SyncJob {
        running: true,
        total: accounts.len(),
        started_at: Some(Utc::now()),
        ..Default::default()
    };
    drop(job);
    let task_state = s.clone();
    s.gateway.tasks.spawn(async move{
        stream::iter(accounts).for_each_concurrent(4,|a|{
            let s=task_state.clone();async move{
                let result=s.gateway.sync_models(a.id).await;
                let mut job=s.model_sync.write().await;job.completed+=1;
                job.results.push(match result{Ok(models)=>json!({"account_id":a.id,"name":a.name,"success":true,"model_count":models.len()}),Err(error)=>json!({"account_id":a.id,"name":a.name,"success":false,"error":error})});
            }
        }).await;
        let mut job=task_state.model_sync.write().await;job.running=false;job.finished_at=Some(Utc::now());
    });
    Json(json!({"running":true}))
}
pub async fn sync_status(State(s): State<AppState>) -> Json<Value> {
    Json(json!(s.model_sync.read().await.clone()))
}
pub async fn discovered(State(s): State<AppState>) -> ApiResult<Json<Value>> {
    let mut models: BTreeMap<(String, String, String), Value> = BTreeMap::new();
    let mut accounts = vec![];
    let configured = s.gateway.store.models().await?;
    for account in s.gateway.scheduler.accounts() {
        accounts.push(json!({"id":account.id,"name":account.name,"enabled":account.enabled,"synced_at":account.model_catalog.as_ref().and_then(|c|c.synced_at),"error":account.model_catalog.as_ref().and_then(|c|c.error.clone())}));
        let Some(catalog) = &account.model_catalog else {
            continue;
        };
        if catalog.synced_at.is_none() {
            continue;
        }
        for model in &catalog.models {
            let reference = ModelRef {
                provider: account.provider.clone(),
                access_kind: account.access_kind.clone(),
                model: model.id.clone(),
            };
            let item=models.entry((reference.provider.clone(),reference.access_kind.clone(),reference.model.clone())).or_insert_with(||{
                let configured_id=configured.iter().find(|m|m.upstream==reference).map(|m|&m.id);
                json!({"model":reference,"display_name":model.display_name,"context_window":model.context_window,"configured_id":configured_id,"accounts":[]})
            });
            if let Some(list) = item["accounts"].as_array_mut() {
                list.push(json!({"id":account.id,"name":account.name,"enabled":account.enabled,"synced_at":catalog.synced_at}));
            }
        }
    }
    accounts.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    Ok(Json(
        json!({"items":models.into_values().collect::<Vec<_>>(),"accounts":accounts,"client_version":xxgate_codex::model_version::current()}),
    ))
}
