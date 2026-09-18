use crate::{
    PgStore,
    accounts::{decode, encode, event_tx},
};
use async_trait::async_trait;
use serde_json::json;
use uuid::Uuid;
use xxgate_core::{
    Error, Result,
    application::ports::ResetStore,
    audit::AuditEvent,
    resets::{ResetCredits, ResetOperation},
};

#[async_trait]
impl ResetStore for PgStore {
    async fn reset_credits(&self, id: Uuid) -> Result<Option<ResetCredits>> {
        sqlx::query("SELECT data FROM account_reset_credits WHERE account_id=$1")
            .bind(id)
            .fetch_optional(&self.pool)
            .await
            .map_err(|_| Error::storage())?
            .as_ref()
            .map(decode)
            .transpose()
    }
    async fn save_reset_credits(&self, id: Uuid, credits: &ResetCredits) -> Result<()> {
        sqlx::query("INSERT INTO account_reset_credits(account_id,data) VALUES($1,$2) ON CONFLICT(account_id) DO UPDATE SET data=excluded.data")
            .bind(id).bind(encode(credits)?).execute(&self.pool).await.map_err(|_| Error::storage())?;
        Ok(())
    }
    async fn reset_operation(
        &self,
        id: Uuid,
        operation: Option<Uuid>,
    ) -> Result<Option<ResetOperation>> {
        sqlx::query("SELECT data FROM account_reset_operations WHERE account_id=$1 AND (($2::uuid IS NULL AND NOT completed) OR id=$2)")
            .bind(id).bind(operation).fetch_optional(&self.pool).await.map_err(|_| Error::storage())?
            .as_ref().map(decode).transpose()
    }
    async fn begin_reset(&self, operation: &ResetOperation) -> Result<()> {
        let mut tx = self.pool.begin().await.map_err(|_| Error::storage())?;
        sqlx::query("INSERT INTO account_reset_operations(id,account_id,data) VALUES($1,$2,$3)")
            .bind(operation.id)
            .bind(operation.account_id)
            .bind(encode(operation)?)
            .execute(&mut *tx)
            .await
            .map_err(|_| Error::storage())?;
        event_tx(
            &mut tx,
            &AuditEvent::new(
                "quota_reset_started",
                "admin",
                None,
                Some(operation.account_id),
                json!({"operation_id":operation.id,"credit_id":operation.credit_id}),
            ),
        )
        .await?;
        tx.commit().await.map_err(|_| Error::storage())
    }
    async fn finish_reset(&self, operation: &ResetOperation) -> Result<()> {
        let mut tx = self.pool.begin().await.map_err(|_| Error::storage())?;
        let changed = sqlx::query("UPDATE account_reset_operations SET completed=true,data=$3 WHERE account_id=$1 AND id=$2 AND NOT completed")
            .bind(operation.account_id).bind(operation.id).bind(encode(operation)?).execute(&mut *tx).await.map_err(|_| Error::storage())?.rows_affected();
        if changed != 1 {
            return Err(Error::conflict());
        }
        event_tx(&mut tx, &AuditEvent::new("quota_reset_finished", "admin", None, Some(operation.account_id), json!({"operation_id":operation.id,"credit_id":operation.credit_id,"result":operation.result}))).await?;
        tx.commit().await.map_err(|_| Error::storage())
    }
}
