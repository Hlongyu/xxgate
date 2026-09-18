use super::{ApiResult, AppState};
use axum::{
    Json,
    extract::{Path, State},
};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;
use xxgate_core::{
    Error, Result,
    groups::{DEFAULT_GROUP_ID, Group},
};

pub(super) async fn validate_selection(s: &AppState, ids: &[Uuid]) -> Result<()> {
    let groups = s.gateway.store.groups().await?;
    if ids.len() > 64 || ids.iter().collect::<std::collections::HashSet<_>>().len() != ids.len() {
        return Err(Error::invalid("Choose at most 64 distinct account groups"));
    }
    if ids.iter().any(|id| !groups.iter().any(|g| g.id == *id)) {
        return Err(Error::new(
            400,
            "group_not_found",
            "One or more selected groups do not exist",
        ));
    }
    Ok(())
}
pub async fn list(State(s): State<AppState>) -> ApiResult<Json<Value>> {
    let accounts = s.gateway.scheduler.accounts();
    let keys = s.gateway.store.keys().await?;
    let items = s.gateway.store.groups().await?.into_iter().map(|g| {
        let members: Vec<_> = accounts.iter().filter(|a| a.group_ids.contains(&g.id)).collect();
        json!({"id":g.id,"name":g.name,"created_at":g.created_at,"is_default":g.id==DEFAULT_GROUP_ID,
            "account_count":members.len(),"enabled_account_count":members.iter().filter(|a| a.enabled).count(),
            "key_count":keys.iter().filter(|k| k.group_id==g.id).count()})
    }).collect::<Vec<_>>();
    Ok(Json(
        json!({"items":items,"default_group_id":DEFAULT_GROUP_ID}),
    ))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Name {
    name: String,
}
pub async fn create(State(s): State<AppState>, Json(input): Json<Name>) -> ApiResult<Json<Group>> {
    let _lock = s.gateway.mutations.lock().await;
    Ok(Json(s.gateway.store.create_group(&input.name).await?))
}
pub async fn rename(
    State(s): State<AppState>,
    Path(id): Path<Uuid>,
    Json(input): Json<Name>,
) -> ApiResult<Json<Group>> {
    let _lock = s.gateway.mutations.lock().await;
    Ok(Json(s.gateway.store.rename_group(id, &input.name).await?))
}
pub async fn remove(State(s): State<AppState>, Path(id): Path<Uuid>) -> ApiResult<Json<Value>> {
    let _lock = s.gateway.mutations.lock().await;
    s.gateway.store.delete_group(id).await?;
    Ok(Json(json!({"deleted":true})))
}
