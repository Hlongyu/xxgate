use super::gateway::{Gateway, read_limited};
use crate::{
    Error, Result,
    protocol::PreparedRequest,
    resets::{ResetCredits, ResetOperation},
};
use chrono::Utc;
use std::sync::Arc;
use tokio::{sync::Mutex, time::Duration};
use uuid::Uuid;

impl Gateway {
    async fn reset_lock(&self, id: Uuid) -> Arc<Mutex<()>> {
        self.reset_locks
            .lock()
            .await
            .entry(id)
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    async fn reset_control(&self, request: PreparedRequest) -> Result<bytes::Bytes> {
        let cancel = self.shutdown.child_token();
        let result = tokio::time::timeout(Duration::from_secs(30), async {
            let mut response = self.transport.send_once(request, cancel.clone()).await?;
            let body = read_limited(&mut response.bytes, 512 * 1024).await?;
            if !(200..300).contains(&response.status) {
                return Err(self.provider.http_error(response.status, &body).0);
            }
            Ok(body)
        })
        .await
        .unwrap_or_else(|_| {
            Err(Error::new(
                504,
                "reset_timeout",
                "额度重置接口超时；执行结果未确认时请重试原操作",
            ))
        });
        cancel.cancel();
        result
    }

    async fn fetch_reset_credits(&self, id: Uuid) -> Result<ResetCredits> {
        let credentials = self.credentials(id, false).await?;
        let account = self.scheduler.account(id).ok_or_else(Error::not_found)?;
        let request = self
            .provider
            .reset_credits_request(&account, &credentials)?;
        let body = self.reset_control(request).await?;
        let credits = self.provider.reset_credits_response(&body)?;
        self.store.save_reset_credits(id, &credits).await?;
        Ok(credits)
    }

    pub async fn collect_reset_credits(&self, id: Uuid) -> Result<ResetCredits> {
        let lock = self.reset_lock(id).await;
        let _guard = lock.lock().await;
        self.fetch_reset_credits(id).await
    }

    pub async fn consume_reset(
        &self,
        id: Uuid,
        operation_id: Uuid,
        expected_credit: &str,
    ) -> Result<ResetOperation> {
        if operation_id.is_nil() || expected_credit.trim().is_empty() || expected_credit.len() > 512
        {
            return Err(Error::invalid("请先查询重置次数，再选择本次操作"));
        }
        let lock = self.reset_lock(id).await;
        let _guard = lock.lock().await;
        let existing = self.store.reset_operation(id, Some(operation_id)).await?;
        let mut operation = if let Some(operation) = existing {
            if operation.credit_id != expected_credit {
                return Err(Error::invalid("同一操作标识不能更换重置凭据"));
            }
            if operation.result.is_some() {
                return Ok(operation);
            }
            operation
        } else {
            if self.store.reset_operation(id, None).await?.is_some() {
                return Err(Error::new(
                    409,
                    "reset_pending",
                    "上一次重置结果尚未确认，请先核实原操作",
                ));
            }
            let credits = self.fetch_reset_credits(id).await?;
            let next = credits
                .next(Utc::now())
                .ok_or_else(|| Error::new(409, "no_reset_credit", "没有尚未过期的可用重置次数"))?;
            if next.id != expected_credit {
                return Err(Error::new(
                    409,
                    "reset_credit_changed",
                    "可用重置列表已变化，请重新查询后操作",
                ));
            }
            ResetOperation {
                id: operation_id,
                account_id: id,
                credit_id: next.id.clone(),
                created_at: Utc::now(),
                result: None,
            }
        };
        // Complete fallible preparation before journaling; after journaling any
        // uncertain response can only retry the original idempotency key.
        let credentials = self.credentials(id, false).await?;
        let account = self.scheduler.account(id).ok_or_else(Error::not_found)?;
        let request = self
            .provider
            .consume_reset_request(&account, &credentials, &operation)?;
        if self
            .store
            .reset_operation(id, Some(operation_id))
            .await?
            .is_none()
        {
            self.store.begin_reset(&operation).await?;
        }
        let result = async {
            let body = self.reset_control(request).await?;
            self.provider.consume_reset_response(&body)
        }
        .await;
        operation.result = Some(match result {
            Ok(result) => result,
            Err(error) => {
                self.store.append_event(&crate::audit::AuditEvent::new(
                    "quota_reset_unconfirmed", "admin", None, Some(id),
                    serde_json::json!({"operation_id":operation.id,"credit_id":operation.credit_id,"error_code":error.code,"http_status":error.status}),
                )).await?;
                return Err(error);
            }
        });
        self.store.finish_reset(&operation).await?;
        Ok(operation)
    }
}
