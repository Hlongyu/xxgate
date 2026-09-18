use super::{
    ApiResult, AppState,
    oauth::{Flow, Start, control, save_credentials},
};
use axum::{
    Json,
    extract::{Path, State},
};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::time::{Duration, Instant};
use uuid::Uuid;
use xxgate_codex::provider::{
    oauth,
    oauth_browser::{BrowserAuthorization, REDIRECT_URI},
};
use xxgate_core::{Error, Result, accounts::Account, protocol::PreparedRequest};

pub async fn start(State(s): State<AppState>, Json(input): Json<Start>) -> ApiResult<Json<Value>> {
    if input.name.trim().is_empty() || input.name.len() > 128 {
        return Err(Error::invalid("请输入账户名称，长度不能超过 128 字节。").into());
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
    let auth = BrowserAuthorization::new();
    let id = Uuid::new_v4();
    let value = json!({"id":id,"status":"pending","authorization_url":auth.authorization_url()?,"redirect_uri":REDIRECT_URI,"expires_in":900});
    let mut flows = s.oauth.lock().await;
    flows.retain(|_, f| f.expires > Instant::now());
    if flows.len() >= 16 {
        return Err(Error::new(
            429,
            "too_many_oauth_flows",
            "待处理的授权过多，请完成或取消已有授权。",
        )
        .into());
    }
    flows.insert(
        id,
        Flow {
            expires: Instant::now() + Duration::from_secs(900),
            value: value.clone(),
            browser: Some((input, auth)),
        },
    );
    Ok(Json(value))
}
#[derive(Deserialize)]
pub struct Callback {
    callback_url: String,
}
pub async fn complete(
    State(s): State<AppState>,
    Path(id): Path<Uuid>,
    Json(callback): Json<Callback>,
) -> ApiResult<Json<Value>> {
    let (input, request) = {
        let mut flows = s.oauth.lock().await;
        let flow = flows
            .get_mut(&id)
            .filter(|f| f.expires > Instant::now())
            .ok_or_else(|| {
                Error::new(
                    410,
                    "oauth_flow_expired",
                    "授权已过期或服务已重启，请重新生成授权链接。",
                )
            })?;
        let (_, auth) = flow.browser.as_ref().ok_or_else(|| {
            Error::new(
                409,
                "oauth_flow_consumed",
                "本次授权已提交，请等待结果；失败后需重新生成链接。",
            )
        })?;
        let code = auth.callback_code(&callback.callback_url)?;
        let request = auth.exchange_request(&code)?;
        // Consume the verifier before awaiting I/O so duplicate submissions can never exchange twice.
        let (input, _) = flow.browser.take().ok_or_else(Error::conflict)?;
        flow.value = json!({"id":id,"status":"exchanging"});
        (input, request)
    };
    let state = s.clone();
    s.gateway.tasks.spawn(async move {
        let result = exchange(&state, id, &input, request).await;
        if let Some(flow) = state.oauth.lock().await.get_mut(&id) {
            flow.value = match result {
                Ok(a) => json!({"id":id,"status":"completed","account":a}),
                Err(e) => {
                    json!({"id":id,"status":"failed","error":{"code":e.code,"message":e.message}})
                }
            };
            flow.expires = Instant::now() + Duration::from_secs(300);
        }
    });
    Ok(Json(json!({"id":id,"status":"exchanging"})))
}
async fn exchange(
    s: &AppState,
    id: Uuid,
    input: &Start,
    request: PreparedRequest,
) -> Result<Account> {
    let response = control(s, request, "browser_token_exchange", id).await?;
    if response.status() != 200 {
        return Err(response.error());
    }
    save_credentials(s, input, oauth::tokens(response.body, None)?).await
}
pub async fn cancel(State(s): State<AppState>, Path(id): Path<Uuid>) -> ApiResult<Json<Value>> {
    let mut flows = s.oauth.lock().await;
    if let Some(flow) = flows.get(&id) {
        if flow.value.get("status").and_then(Value::as_str) == Some("exchanging") {
            return Err(Error::new(
                409,
                "oauth_exchange_in_progress",
                "正在兑换凭证，请等待结果。",
            )
            .into());
        }
        if flow.browser.is_some() {
            flows.remove(&id);
        }
    }
    Ok(Json(json!({"cancelled":true})))
}
