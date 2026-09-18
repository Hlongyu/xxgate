use super::{ApiResult, AppState};
use axum::{
    Json,
    extract::{Path, Query, State},
};
use chrono::Utc;
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::atomic::Ordering;
use uuid::Uuid;
use xxgate_core::{
    Error,
    access::{GatewayKey, random_secret, secret_hash},
    accounts::{Account, ClientProfile, Credentials, DisableReason},
    pricing::Price,
    providers::ModelSpec,
    reports::{RequestFilter, UsageFilter},
    settings::RuntimeSettings,
};

#[derive(Deserialize, Default)]
pub struct ReportQuery {
    offset: Option<i64>,
    limit: Option<i64>,
}
pub async fn dashboard(
    State(s): State<AppState>,
    Query(q): Query<UsageFilter>,
) -> ApiResult<Json<Value>> {
    let mut result = s.gateway.store.dashboard(&q).await?;
    result["runtime"] = json!({"queue":s.gateway.scheduler.stats(),"memory_bytes":s.gateway.memory.used(),"config_version":s.gateway.settings.current().version});
    Ok(Json(result))
}
pub async fn accounts(State(s): State<AppState>) -> ApiResult<Json<Value>> {
    let mut items = Vec::new();
    let now = Utc::now();
    let stale = s.gateway.settings.current().quota_stale_after_secs;
    for account in s.gateway.store.accounts().await? {
        let windows = s
            .gateway
            .store
            .quotas(account.id)
            .await?
            .into_iter()
            .map(|w| {
                let is_stale = (now - w.observed_at).num_seconds() > stale as i64
                    || w.resets_at.is_some_and(|at| at <= now);
                json!({"window":w,"stale":is_stale})
            })
            .collect::<Vec<_>>();
        let spending = s
            .gateway
            .store
            .account_spending(account.id, now, stale)
            .await?;
        let resets = super::resets::view(&s, account.id).await?;
        items.push(json!({"account":account,"quotas":windows,"spending":spending,"resets":resets}));
    }
    Ok(Json(
        json!({"items":items,"runtime":s.gateway.scheduler.stats()}),
    ))
}
pub async fn account_detail(
    State(s): State<AppState>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<Value>> {
    let a = s
        .gateway
        .scheduler
        .account(id)
        .ok_or_else(Error::not_found)?;
    Ok(Json(
        json!({"account":a,"quotas":s.gateway.store.quotas(id).await?,"statistics":s.gateway.store.dashboard(&UsageFilter{account_id:Some(id),..Default::default()}).await?,"spending":s.gateway.store.account_spending(id,Utc::now(),s.gateway.settings.current().quota_stale_after_secs).await?}),
    ))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateAccount {
    #[serde(default)]
    codex_only: bool,
    name: String,
    #[serde(default = "xxgate_core::groups::default_group_ids")]
    group_ids: Vec<Uuid>,
    credentials: Credentials,
    upstream_account_id: Option<String>,
    upstream_base_url: Option<String>,
    max_inflight: Option<u32>,
    #[serde(default)]
    models: Vec<String>,
}
pub async fn create_account(
    State(s): State<AppState>,
    Json(input): Json<CreateAccount>,
) -> ApiResult<Json<Account>> {
    input.credentials.validate()?;
    let upstream_id = if let Some(id) = input.upstream_account_id {
        id
    } else {
        xxgate_codex::provider::oauth::account_details(&input.credentials)?.account_id
    };
    let account = Account {
        id: Uuid::new_v4(),
        group_ids: input.group_ids,
        name: input.name,
        provider: "openai".into(),
        access_kind: "codex_oauth".into(),
        enabled: false,
        codex_only: input.codex_only,
        disable_reason: Some(DisableReason::AdminDisabled),
        max_inflight: input
            .max_inflight
            .unwrap_or(s.gateway.settings.current().default_account_concurrency),
        upstream_account_id: upstream_id,
        upstream_base_url: input
            .upstream_base_url
            .unwrap_or_else(|| "https://chatgpt.com/backend-api/codex".into()),
        models: input.models,
        models_restricted: false,
        model_catalog: None,
        profile: ClientProfile::default(),
        version: 0,
        credential_version: 0,
        credential_expires_at: input.credentials.expires_at,
        created_at: Utc::now(),
        updated_at: Utc::now(),
    };
    account.validate(s.allow_http)?;
    let _lock = s.gateway.mutations.lock().await;
    super::groups::validate_selection(&s, &account.group_ids).await?;
    if s.gateway
        .scheduler
        .accounts()
        .iter()
        .any(|a| a.upstream_account_id == account.upstream_account_id)
    {
        return Err(Error::new(
            409,
            "account_already_exists",
            "This ChatGPT account is already managed",
        )
        .into());
    }
    let encrypted = s.gateway.cipher.encrypt(account.id, &input.credentials)?;
    let account = s
        .gateway
        .store
        .put_account(&account, Some(&encrypted), None)
        .await?;
    s.gateway.scheduler.update_account(account.clone());
    super::catalog::schedule(&s, account.id);
    Ok(Json(account))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AccountUpdate {
    codex_only: Option<bool>,
    version: i64,
    group_ids: Option<Vec<Uuid>>,
    name: String,
    max_inflight: u32,
    models: Vec<String>,
    #[serde(default)]
    models_restricted: bool,
    user_agent: Option<String>,
    tls_backend: Option<String>,
}
pub async fn update_account(
    State(s): State<AppState>,
    Path(id): Path<Uuid>,
    Json(input): Json<AccountUpdate>,
) -> ApiResult<Json<Account>> {
    let _lock = s.gateway.mutations.lock().await;
    let mut a = s
        .gateway
        .scheduler
        .account(id)
        .ok_or_else(Error::not_found)?;
    if a.version != input.version {
        return Err(Error::conflict().into());
    }
    if let Some(codex_only) = input.codex_only {
        a.codex_only = codex_only;
    }
    a.name = input.name;
    a.max_inflight = input.max_inflight;
    a.models = input.models;
    a.models_restricted = input.models_restricted;
    if let Some(group_ids) = input.group_ids {
        super::groups::validate_selection(&s, &group_ids).await?;
        a.group_ids = group_ids;
    }
    if let Some(ua) = input.user_agent {
        a.profile.user_agent = ua;
    }
    if let Some(tls) = input.tls_backend {
        a.profile.tls_backend = tls;
    }
    a.validate(s.allow_http)?;
    s.gateway.scheduler.block_account(id);
    match s
        .gateway
        .store
        .put_account(&a, None, Some(input.version))
        .await
    {
        Ok(a) => {
            s.gateway.scheduler.update_account(a.clone());
            Ok(Json(a))
        }
        Err(e) => {
            if let Ok(accounts) = s.gateway.store.accounts().await {
                for account in accounts {
                    s.gateway.scheduler.update_account(account);
                }
            } else {
                s.gateway.scheduler.set_paused(true);
            }
            Err(e.into())
        }
    }
}
#[derive(Deserialize)]
pub struct Enabled {
    enabled: bool,
}
pub async fn enable_account(
    State(s): State<AppState>,
    Path(id): Path<Uuid>,
    Json(input): Json<Enabled>,
) -> ApiResult<Json<Account>> {
    Ok(Json(
        s.gateway
            .set_enabled(
                id,
                input.enabled,
                (!input.enabled).then_some(DisableReason::AdminDisabled),
                "admin",
            )
            .await?,
    ))
}
pub async fn refresh_account(
    State(s): State<AppState>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<Value>> {
    s.gateway.credentials(id, true).await?;
    Ok(Json(json!({"account":s.gateway.scheduler.account(id)})))
}
pub async fn refresh_quota(
    State(s): State<AppState>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<Value>> {
    Ok(Json(json!({"quotas":s.gateway.collect_quotas(id).await?})))
}
pub async fn keys(State(s): State<AppState>) -> ApiResult<Json<Value>> {
    Ok(Json(json!({"items":s.gateway.store.keys().await?})))
}

pub async fn search_price(
    State(s): State<AppState>,
) -> ApiResult<Json<xxgate_core::pricing::SearchPrice>> {
    Ok(Json(s.gateway.store.search_price().await?))
}
pub async fn save_search_price(
    State(s): State<AppState>,
    Json(price): Json<xxgate_core::pricing::SearchPrice>,
) -> ApiResult<Json<xxgate_core::pricing::SearchPrice>> {
    Ok(Json(s.gateway.store.put_search_price(&price).await?))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Name {
    name: String,
    #[serde(default = "xxgate_core::groups::default_group_id")]
    group_id: Uuid,
}
pub async fn create_key(
    State(s): State<AppState>,
    Json(input): Json<Name>,
) -> ApiResult<Json<Value>> {
    if input.name.trim().is_empty() || input.name.len() > 128 {
        return Err(Error::invalid("Key name must contain 1 to 128 bytes").into());
    }
    let secret = format!("sk-{}", random_secret(32));
    let k = GatewayKey {
        id: Uuid::new_v4(),
        group_id: input.group_id,
        name: input.name,
        prefix: secret.chars().take(11).collect(),
        enabled: true,
        created_at: Utc::now(),
        last_used_at: None,
    };
    let _lock = s.gateway.mutations.lock().await;
    super::groups::validate_selection(&s, &[k.group_id]).await?;
    s.gateway.store.put_key(&k, &secret_hash(&secret)).await?;
    s.gateway.scheduler.update_key(k.clone());
    Ok(Json(json!({"key":k,"secret":secret})))
}
pub async fn enable_key(
    State(s): State<AppState>,
    Path(id): Path<Uuid>,
    Json(input): Json<Enabled>,
) -> ApiResult<Json<Value>> {
    let _lock = s.gateway.mutations.lock().await;
    s.gateway.scheduler.block_key(id);
    match s.gateway.store.set_key_enabled(id, input.enabled).await {
        Ok(key) => s.gateway.scheduler.update_key(key),
        Err(error) => {
            reconcile_keys(&s).await;
            return Err(error.into());
        }
    }
    Ok(Json(json!({"enabled":input.enabled})))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KeyUpdate {
    name: String,
    group_id: Uuid,
}
pub async fn update_key(
    State(s): State<AppState>,
    Path(id): Path<Uuid>,
    Json(input): Json<KeyUpdate>,
) -> ApiResult<Json<GatewayKey>> {
    if input.name.trim().is_empty() || input.name.len() > 128 {
        return Err(Error::invalid("Key name must contain 1 to 128 bytes").into());
    }
    let _lock = s.gateway.mutations.lock().await;
    super::groups::validate_selection(&s, &[input.group_id]).await?;
    s.gateway.scheduler.block_key(id);
    match s
        .gateway
        .store
        .update_key(id, &input.name, input.group_id)
        .await
    {
        Ok(key) => {
            s.gateway.scheduler.update_key(key.clone());
            Ok(Json(key))
        }
        Err(error) => {
            reconcile_keys(&s).await;
            Err(error.into())
        }
    }
}
async fn reconcile_keys(s: &AppState) {
    match s.gateway.store.keys().await {
        Ok(keys) => {
            for key in keys {
                s.gateway.scheduler.update_key(key);
            }
        }
        Err(_) => s.gateway.scheduler.set_paused(true),
    }
}
pub async fn settings(State(s): State<AppState>) -> Json<RuntimeSettings> {
    Json(s.gateway.settings.current())
}
pub async fn request_errors(
    State(s): State<AppState>,
    Query(filter): Query<xxgate_core::reports::ErrorFilter>,
) -> ApiResult<Json<Value>> {
    Ok(Json(s.gateway.store.request_errors(&filter).await?))
}
pub async fn save_settings(
    State(s): State<AppState>,
    Json(input): Json<RuntimeSettings>,
) -> ApiResult<Json<RuntimeSettings>> {
    input.validate()?;
    let _lock = s.gateway.mutations.lock().await;
    match s.gateway.store.save_settings(&input, input.version).await {
        Ok(next) => {
            s.gateway.settings.publish(next.clone());
            s.gateway.scheduler.wake();
            Ok(Json(next))
        }
        Err(e) => {
            // A commit can succeed even if its acknowledgement was lost. Reconcile before returning.
            if let Ok(persisted) = s.gateway.store.settings().await {
                s.gateway.settings.publish(persisted);
                s.gateway.scheduler.wake();
            } else {
                s.gateway.scheduler.set_paused(true);
            }
            Err(e.into())
        }
    }
}
pub async fn models(State(s): State<AppState>) -> ApiResult<Json<Value>> {
    Ok(Json(json!({"items":s.gateway.store.models().await?})))
}
pub async fn save_model(
    State(s): State<AppState>,
    Json(input): Json<ModelSpec>,
) -> ApiResult<Json<ModelSpec>> {
    let _lock = s.gateway.mutations.lock().await;
    Ok(Json(s.gateway.store.put_model(&input).await?))
}
pub async fn prices(State(s): State<AppState>) -> ApiResult<Json<Value>> {
    Ok(Json(json!({"items":s.gateway.store.prices().await?})))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PriceInput {
    #[serde(default)]
    version: i64,
    model: xxgate_core::types::ModelRef,
    standard: xxgate_core::pricing::Rates,
    fast_multiplier: rust_decimal::Decimal,
}
pub async fn save_price(
    State(s): State<AppState>,
    Json(input): Json<PriceInput>,
) -> ApiResult<Json<Price>> {
    if !s
        .gateway
        .store
        .models()
        .await?
        .iter()
        .any(|m| m.upstream == input.model)
    {
        return Err(Error::invalid("Configure the model before setting its price").into());
    }
    Ok(Json(
        s.gateway
            .store
            .put_price(&Price {
                version: input.version,
                model: input.model,
                standard: input.standard,
                fast_multiplier: Some(input.fast_multiplier),
            })
            .await?,
    ))
}
pub async fn requests(
    State(s): State<AppState>,
    Query(q): Query<RequestFilter>,
) -> ApiResult<Json<Value>> {
    Ok(Json(s.gateway.store.requests(&q).await?))
}
pub async fn request_detail(
    State(s): State<AppState>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<Value>> {
    Ok(Json(s.gateway.store.request_detail(id).await?))
}
pub async fn queue(State(s): State<AppState>) -> Json<Value> {
    Json(json!({"queue":s.gateway.scheduler.stats(),"memory_bytes":s.gateway.memory.used()}))
}
pub async fn audit(
    State(s): State<AppState>,
    Query(q): Query<ReportQuery>,
) -> ApiResult<Json<Value>> {
    Ok(Json(
        s.gateway
            .store
            .audit_events(q.offset.unwrap_or(0), q.limit.unwrap_or(50))
            .await?,
    ))
}
pub async fn cleanup_status(State(s): State<AppState>) -> Json<Value> {
    Json(s.cleanup.read().await.clone())
}
pub async fn cleanup(State(s): State<AppState>) -> Json<Value> {
    let task_state = s.clone();
    s.gateway.tasks.spawn(async move {
        crate::workers::cleanup_once(&task_state).await;
    });
    Json(json!({"scheduled":true}))
}
pub async fn metrics(State(s): State<AppState>) -> String {
    let q = s.gateway.scheduler.stats();
    format!(
        "# TYPE xxgate_requests_accepted_total counter\nxxgate_requests_accepted_total {}\n# TYPE xxgate_requests_completed_total counter\nxxgate_requests_completed_total {}\n# TYPE xxgate_requests_failed_total counter\nxxgate_requests_failed_total {}\n# TYPE xxgate_queue_requests gauge\nxxgate_queue_requests {}\n# TYPE xxgate_inflight_requests gauge\nxxgate_inflight_requests {}\n# TYPE xxgate_memory_reserved_bytes gauge\nxxgate_memory_reserved_bytes {}\n# TYPE xxgate_config_version gauge\nxxgate_config_version {}\n",
        s.gateway.accepted.load(Ordering::Relaxed),
        s.gateway.completed.load(Ordering::Relaxed),
        s.gateway.failed.load(Ordering::Relaxed),
        q.queued,
        q.inflight,
        s.gateway.memory.used(),
        s.gateway.settings.current().version
    )
}
