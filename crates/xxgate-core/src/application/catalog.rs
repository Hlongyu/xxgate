use super::gateway::{Gateway, read_limited};
use crate::{
    Error, Result,
    audit::AuditEvent,
    providers::{Capabilities, DiscoveredModel, ModelSpec},
    types::ModelRef,
};
use chrono::Utc;
use serde_json::json;
use std::sync::Arc;
use tokio::{sync::Mutex, time::Duration};
use uuid::Uuid;

impl Gateway {
    pub async fn sync_models(&self, id: Uuid) -> Result<Vec<DiscoveredModel>> {
        let lock = self
            .model_sync_locks
            .lock()
            .await
            .entry(id)
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone();
        let _sync = lock.lock().await;
        let result = async {
            let credentials = self.credentials(id, false).await?;
            let account = self.scheduler.account(id).ok_or_else(Error::not_found)?;
            let request = self.provider.models_request(&account, &credentials)?;
            let cancel = self.shutdown.child_token();
            let result = tokio::time::timeout(Duration::from_secs(30), async {
                let mut response = self.transport.send_once(request, cancel.clone()).await?;
                let body = read_limited(&mut response.bytes, 8 * 1024 * 1024).await?;
                if !(200..300).contains(&response.status) {
                    let (error, reason) = self.provider.http_error(response.status, &body);
                    if let Some(reason) = reason {
                        self.disable_observed(id, account.version, reason).await?;
                    }
                    return Err(error);
                }
                self.provider.models_response(&body)
            })
            .await
            .unwrap_or_else(|_| {
                Err(Error::new(
                    504,
                    "model_sync_timeout",
                    "获取上游模型目录超时。",
                ))
            });
            cancel.cancel();
            result
        }
        .await;
        let _lock = self.mutations.lock().await;
        let account = self.scheduler.account(id).ok_or_else(Error::not_found)?;
        let mut catalog = account.model_catalog.clone().unwrap_or_default();
        catalog.attempted_at = Some(Utc::now());
        match &result {
            Ok(models) => {
                let mut configured = self.store.models().await?;
                for model in models {
                    let upstream = ModelRef {
                        provider: account.provider.clone(),
                        access_kind: account.access_kind.clone(),
                        model: model.id.clone(),
                    };
                    if !configured
                        .iter()
                        .any(|m| m.id == model.id || m.upstream == upstream)
                    {
                        configured.push(
                            self.store
                                .put_model(&ModelSpec {
                                    id: model.id.clone(),
                                    upstream,
                                    enabled: true,
                                    capabilities: Capabilities::default(),
                                    version: 0,
                                })
                                .await?,
                        );
                    }
                }
                catalog.models = models.clone();
                catalog.synced_at = Some(Utc::now());
                catalog.error = None;
            }
            Err(error) => {
                catalog.error = Some(error.clone());
            }
        }
        let updated = self.store.save_model_catalog(id, &catalog).await?;
        self.scheduler.update_account(updated);
        self.store.append_event(&AuditEvent::new(if result.is_ok(){"models_synchronized"}else{"models_sync_failed"},"model_catalog",None,Some(id),json!({"model_count":result.as_ref().ok().map(Vec::len),"error_code":result.as_ref().err().map(|e|&e.code)}))).await?;
        result
    }
}
