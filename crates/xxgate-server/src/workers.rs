use crate::http::AppState;
use futures::{StreamExt, stream};
use serde_json::json;
use tokio::time::{Duration, Instant};
use xxgate_core::accounts::DisableReason;

pub fn start(state: AppState) {
    let quotas = state.clone();
    state.gateway.tasks.spawn(async move {
        let mut config = quotas.gateway.settings.subscribe();
        let mut last = Instant::now();
        loop {
            let interval = config.borrow_and_update().quota_poll_interval_secs;
            tokio::select! {
                _=quotas.gateway.shutdown.cancelled()=>break,
                _=config.changed()=>continue,
                _=tokio::time::sleep_until(last+Duration::from_secs(interval))=>{}
            }
            let accounts = quotas
                .gateway
                .scheduler
                .accounts()
                .into_iter()
                .filter(|a| a.disable_reason != Some(DisableReason::OauthInvalid))
                .collect::<Vec<_>>();
            let polls = stream::iter(accounts).for_each_concurrent(4, |a| {
                let gateway = quotas.gateway.clone();
                async move {
                    if let Err(error) = gateway.collect_quotas(a.id).await {
                        tracing::warn!(account_id=%a.id,code=%error.code,"quota collection failed");
                    }
                }
            });
            tokio::select! {_=quotas.gateway.shutdown.cancelled()=>break,_=polls=>{}}
            last = Instant::now();
        }
    });
    let cleanup = state.clone();
    state.gateway.tasks.spawn(async move{
        let mut config=cleanup.gateway.settings.subscribe();
        loop{
            let snapshot=config.borrow_and_update().clone();
            cleanup_once(&cleanup).await;
            loop{
                tokio::select!{
                    _=cleanup.gateway.shutdown.cancelled()=>return,
                    _=tokio::time::sleep(Duration::from_secs(3600))=>break,
                    _=config.changed()=>{
                        let next=config.borrow_and_update().clone();
                        if next.request_retention_days!=snapshot.request_retention_days||next.audit_retention_days!=snapshot.audit_retention_days{break;}
                    }
                }
            }
        }
    });
}
pub async fn cleanup_once(state: &AppState) {
    let settings = state.gateway.settings.current();
    {
        let mut status = state.cleanup.write().await;
        if status.get("state").and_then(|v| v.as_str()) == Some("running") {
            return;
        }
        *status = json!({"state":"running","config_version":settings.version,"started_at":chrono::Utc::now()});
    }
    let result = state.gateway.store.cleanup(&settings).await;
    *state.cleanup.write().await = match result {
        Ok(mut value) => {
            value["state"] = json!("completed");
            value
        }
        Err(error) => {
            json!({"state":"failed","config_version":settings.version,"code":error.code,"finished_at":chrono::Utc::now()})
        }
    };
}
