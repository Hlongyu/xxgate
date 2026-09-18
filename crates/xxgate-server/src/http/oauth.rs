use super::{ApiResult, AppState};
use axum::{
    Json,
    extract::{Path, State},
};
use chrono::Utc;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::time::{Duration, Instant};
use uuid::Uuid;
use xxgate_codex::provider::oauth;
use xxgate_codex::provider::oauth_response::OAuthResponse;
use xxgate_core::{
    Error, Result,
    accounts::{Account, ClientProfile, DisableReason},
    application::gateway::read_limited,
    audit::AuditEvent,
    protocol::PreparedRequest,
};

pub(crate) struct Flow {
    pub expires: Instant,
    pub value: Value,
    pub browser: Option<(
        Start,
        xxgate_codex::provider::oauth_browser::BrowserAuthorization,
    )>,
}
#[derive(Clone, Deserialize)]
pub struct Start {
    pub(super) name: String,
    pub(super) account_id: Option<Uuid>,
    #[serde(default = "xxgate_core::groups::default_group_ids")]
    pub(super) group_ids: Vec<Uuid>,
}
async fn record_failure(s: &AppState, flow_id: Uuid, stage: &str, details: Value) {
    tracing::warn!(%flow_id,stage,diagnostics=%details,"OAuth control request failed");
    if s.gateway
        .store
        .append_event(&AuditEvent::new(
            "oauth_request_failed",
            "oauth",
            None,
            None,
            json!({"flow_id":flow_id,"stage":stage,"diagnostics":details}),
        ))
        .await
        .is_err()
    {
        tracing::error!(%flow_id,stage,"OAuth diagnostic persistence failed");
    }
}
pub(super) async fn control(
    s: &AppState,
    request: PreparedRequest,
    stage: &str,
    flow_id: Uuid,
) -> Result<OAuthResponse> {
    let cancel = s.gateway.shutdown.child_token();
    let result = tokio::time::timeout(Duration::from_secs(30), async {
        let mut r = s
            .gateway
            .transport
            .send_once(request, cancel.clone())
            .await?;
        let body = read_limited(&mut r.bytes, 128 * 1024).await?;
        Ok(OAuthResponse::decode(r.status, &r.headers, &body))
    })
    .await
    .unwrap_or_else(|_| Err(Error::new(504, "oauth_timeout", "OAuth request timed out")));
    cancel.cancel();
    match &result {
        Ok(response)
            if response.status() != 200 && !(stage == "device_poll" && response.pending()) =>
        {
            record_failure(s, flow_id, stage, json!(response.diagnostics)).await;
        }
        Err(error) => record_failure(s, flow_id, stage, json!({"code":error.code})).await,
        _ => {}
    }
    result
}
pub async fn start(State(s): State<AppState>, Json(input): Json<Start>) -> ApiResult<Json<Value>> {
    if input.name.trim().is_empty() || input.name.len() > 128 {
        return Err(Error::invalid("An account name is required").into());
    }
    if input.account_id.is_none() {
        super::groups::validate_selection(&s, &input.group_ids).await?;
    }
    if let Some(id) = input.account_id {
        s.gateway
            .scheduler
            .account(id)
            .ok_or_else(Error::not_found)?;
    }
    let id = Uuid::new_v4();
    {
        let mut flows = s.oauth.lock().await;
        flows.retain(|_, flow| flow.expires > Instant::now());
        if flows.len() >= 16 {
            return Err(Error::new(
                429,
                "too_many_oauth_flows",
                "Too many pending authorizations",
            )
            .into());
        }
        flows.insert(
            id,
            Flow {
                expires: Instant::now() + Duration::from_secs(900),
                value: json!({"status":"starting"}),
                browser: None,
            },
        );
    }
    let result = control(&s, oauth::device_start()?, "device_start", id).await;
    let response = match result {
        Ok(r) => r,
        Err(e) => {
            s.oauth.lock().await.remove(&id);
            return Err(e.into());
        }
    };
    if response.status() != 200 {
        s.oauth.lock().await.remove(&id);
        return Err(response.error().into());
    }
    let code: oauth::DeviceCode = match serde_json::from_value(response.body.clone()) {
        Ok(code) => code,
        Err(_) => {
            s.oauth.lock().await.remove(&id);
            record_failure(&s, id, "device_start", json!(response.diagnostics)).await;
            return Err(response.error().into());
        }
    };
    let value = json!({"id":id,"status":"pending","user_code":code.user_code,"verification_uri":"https://auth.openai.com/codex/device","expires_in":900});
    if let Some(flow) = s.oauth.lock().await.get_mut(&id) {
        flow.value = value.clone();
    }
    let task_state = s.clone();
    s.gateway.tasks.spawn(async move{
        let result=poll(&task_state,&input,&code,id).await;
        let mut flows=task_state.oauth.lock().await;
        if let Some(flow)=flows.get_mut(&id){
            flow.value=match result{Ok(account)=>json!({"id":id,"status":"completed","account":account}),Err(error)=>json!({"id":id,"status":"failed","error":{"code":error.code,"message":error.message}})};
            flow.expires=Instant::now()+Duration::from_secs(300);
        }
    });
    Ok(Json(value))
}
async fn poll(
    s: &AppState,
    input: &Start,
    code: &oauth::DeviceCode,
    flow_id: Uuid,
) -> Result<Account> {
    let deadline = Instant::now() + Duration::from_secs(900);
    let approved = loop {
        tokio::select! {
            _=s.gateway.shutdown.cancelled()=>return Err(Error::cancelled()),
            _=tokio::time::sleep_until(deadline)=>return Err(Error::new(408,"oauth_expired","Device authorization expired")),
            _=tokio::time::sleep(Duration::from_secs(code.poll_seconds()))=>{}
        }
        let response = control(s, oauth::device_poll(code)?, "device_poll", flow_id).await?;
        if response.status() == 200 {
            break response.body;
        }
        if !response.pending() {
            return Err(response.error());
        }
    };
    let response = control(
        s,
        oauth::exchange_device(&approved)?,
        "token_exchange",
        flow_id,
    )
    .await?;
    if response.status() != 200 {
        return Err(response.error());
    }
    let credentials = oauth::tokens(response.body, None)?;
    save_credentials(s, input, credentials).await
}
pub(super) async fn save_credentials(
    s: &AppState,
    input: &Start,
    credentials: xxgate_core::accounts::Credentials,
) -> Result<Account> {
    credentials.validate()?;
    let details = oauth::account_details(&credentials)?;
    let _lock = s.gateway.mutations.lock().await;
    let a = if let Some(id) = input.account_id {
        let account = s
            .gateway
            .scheduler
            .account(id)
            .ok_or_else(Error::not_found)?;
        if account.upstream_account_id != details.account_id {
            return Err(Error::invalid(
                "Authorization belongs to a different ChatGPT account",
            ));
        }
        s.gateway
            .store
            .update_credentials(
                id,
                &s.gateway.cipher.encrypt(id, &credentials)?,
                credentials.expires_at,
                account.credential_version,
            )
            .await?
    } else {
        if s.gateway
            .scheduler
            .accounts()
            .iter()
            .any(|a| a.upstream_account_id == details.account_id)
        {
            return Err(Error::new(
                409,
                "account_already_exists",
                "This ChatGPT account is already managed; authorize it from its account settings",
            ));
        }
        let a = Account {
            id: Uuid::new_v4(),
            group_ids: input.group_ids.clone(),
            name: input.name.clone(),
            provider: "openai".into(),
            access_kind: "codex_oauth".into(),
            enabled: false,
            codex_only: false,
            disable_reason: Some(DisableReason::AdminDisabled),
            max_inflight: s.gateway.settings.current().default_account_concurrency,
            upstream_account_id: details.account_id,
            upstream_base_url: "https://chatgpt.com/backend-api/codex".into(),
            models: vec![],
            models_restricted: false,
            model_catalog: None,
            profile: ClientProfile::default(),
            version: 0,
            credential_version: 0,
            credential_expires_at: credentials.expires_at,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        a.validate(s.allow_http)?;
        s.gateway
            .store
            .put_account(
                &a,
                Some(&s.gateway.cipher.encrypt(a.id, &credentials)?),
                None,
            )
            .await?
    };
    s.gateway.scheduler.update_account(a.clone());
    super::catalog::schedule(s, a.id);
    Ok(a)
}
pub async fn status(State(s): State<AppState>, Path(id): Path<Uuid>) -> ApiResult<Json<Value>> {
    let flows = s.oauth.lock().await;
    let flow = flows
        .get(&id)
        .filter(|f| f.expires > Instant::now())
        .ok_or_else(Error::not_found)?;
    Ok(Json(flow.value.clone()))
}
