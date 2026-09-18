use super::accounts::{PgStore, event_tx};
use async_trait::async_trait;
use sqlx::Row;
use uuid::Uuid;
use xxgate_core::{
    Error, Result,
    access::{AdminSession, GatewayKey},
    application::ports::AccessStore,
    audit::AuditEvent,
};

fn key(row: &sqlx::postgres::PgRow) -> Result<GatewayKey> {
    Ok(GatewayKey {
        id: row.try_get("id").map_err(|_| Error::storage())?,
        group_id: row.try_get("group_id").map_err(|_| Error::storage())?,
        name: row.try_get("name").map_err(|_| Error::storage())?,
        prefix: row.try_get("prefix").map_err(|_| Error::storage())?,
        enabled: row.try_get("enabled").map_err(|_| Error::storage())?,
        created_at: row.try_get("created_at").map_err(|_| Error::storage())?,
        last_used_at: row.try_get("last_used_at").map_err(|_| Error::storage())?,
    })
}
#[async_trait]
impl AccessStore for PgStore {
    async fn initialize_admin(&self, password_hash: &str) -> Result<bool> {
        Ok(sqlx::query("INSERT INTO administrators(singleton,password_hash) VALUES(TRUE,$1) ON CONFLICT DO NOTHING").bind(password_hash).execute(&self.pool).await.map_err(|_| Error::storage())?.rows_affected() > 0)
    }
    async fn admin_password_hash(&self) -> Result<Option<String>> {
        sqlx::query_scalar("SELECT password_hash FROM administrators WHERE singleton=TRUE")
            .fetch_optional(&self.pool)
            .await
            .map_err(|_| Error::storage())
    }
    async fn put_admin_session(&self, hash: &str, session: &AdminSession) -> Result<()> {
        sqlx::query("INSERT INTO admin_sessions(secret_hash,id,expires_at) VALUES($1,$2,$3)")
            .bind(hash)
            .bind(session.id)
            .bind(session.expires_at)
            .execute(&self.pool)
            .await
            .map_err(|_| Error::storage())?;
        Ok(())
    }
    async fn admin_session(&self, hash: &str) -> Result<Option<AdminSession>> {
        let row = sqlx::query(
            "SELECT id,expires_at FROM admin_sessions WHERE secret_hash=$1 AND expires_at>now()",
        )
        .bind(hash)
        .fetch_optional(&self.pool)
        .await
        .map_err(|_| Error::storage())?;
        row.map(|r| {
            Ok(AdminSession {
                id: r.try_get("id").map_err(|_| Error::storage())?,
                expires_at: r.try_get("expires_at").map_err(|_| Error::storage())?,
            })
        })
        .transpose()
    }
    async fn delete_admin_session(&self, hash: &str) -> Result<()> {
        sqlx::query("DELETE FROM admin_sessions WHERE secret_hash=$1")
            .bind(hash)
            .execute(&self.pool)
            .await
            .map_err(|_| Error::storage())?;
        Ok(())
    }
    async fn keys(&self) -> Result<Vec<GatewayKey>> {
        sqlx::query("SELECT id,group_id,name,prefix,enabled,created_at,last_used_at FROM api_keys ORDER BY created_at DESC").fetch_all(&self.pool).await.map_err(|_| Error::storage())?.iter().map(key).collect()
    }
    async fn put_key(&self, k: &GatewayKey, hash: &str) -> Result<()> {
        let mut tx = self.pool.begin().await.map_err(|_| Error::storage())?;
        super::groups::validate_groups(&mut tx, &[k.group_id]).await?;
        sqlx::query("INSERT INTO api_keys(id,name,prefix,secret_hash,enabled,created_at,group_id) VALUES($1,$2,$3,$4,$5,$6,$7)").bind(k.id).bind(&k.name).bind(&k.prefix).bind(hash).bind(k.enabled).bind(k.created_at).bind(k.group_id).execute(&mut *tx).await.map_err(|_| Error::storage())?;
        event_tx(
            &mut tx,
            &AuditEvent::new(
                "key_created",
                "admin",
                None,
                None,
                serde_json::json!({"key_id":k.id,"name":k.name,"prefix":k.prefix,"group_id":k.group_id}),
            ),
        )
        .await?;
        tx.commit().await.map_err(|_| Error::storage())?;
        Ok(())
    }
    async fn key_by_hash(&self, hash: &str) -> Result<Option<GatewayKey>> {
        let row = sqlx::query("SELECT id,group_id,name,prefix,enabled,created_at,last_used_at FROM api_keys WHERE secret_hash=$1 AND enabled=TRUE").bind(hash).fetch_optional(&self.pool).await.map_err(|_| Error::storage())?;
        if let Some(r) = row {
            let k = key(&r)?;
            sqlx::query("UPDATE api_keys SET last_used_at=now() WHERE id=$1 AND (last_used_at IS NULL OR last_used_at<now()-interval '1 minute')").bind(k.id).execute(&self.pool).await.map_err(|_| Error::storage())?;
            Ok(Some(k))
        } else {
            Ok(None)
        }
    }
    async fn set_key_enabled(&self, id: Uuid, enabled: bool) -> Result<GatewayKey> {
        let mut tx = self.pool.begin().await.map_err(|_| Error::storage())?;
        let row = sqlx::query("UPDATE api_keys SET enabled=$2 WHERE id=$1 RETURNING *")
            .bind(id)
            .bind(enabled)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|_| Error::storage())?
            .ok_or_else(Error::not_found)?;
        let k = key(&row)?;
        event_tx(
            &mut tx,
            &AuditEvent::new(
                "key_updated",
                "admin",
                None,
                None,
                serde_json::json!({"key_id":id,"enabled":enabled,"group_id":k.group_id}),
            ),
        )
        .await?;
        tx.commit().await.map_err(|_| Error::storage())?;
        Ok(k)
    }
    async fn update_key(&self, id: Uuid, name: &str, group_id: Uuid) -> Result<GatewayKey> {
        let mut tx = self.pool.begin().await.map_err(|_| Error::storage())?;
        super::groups::validate_groups(&mut tx, &[group_id]).await?;
        let previous: Uuid =
            sqlx::query_scalar("SELECT group_id FROM api_keys WHERE id=$1 FOR UPDATE")
                .bind(id)
                .fetch_optional(&mut *tx)
                .await
                .map_err(|_| Error::storage())?
                .ok_or_else(Error::not_found)?;
        let row = sqlx::query("UPDATE api_keys SET name=$2,group_id=$3 WHERE id=$1 RETURNING *")
            .bind(id)
            .bind(name)
            .bind(group_id)
            .fetch_one(&mut *tx)
            .await
            .map_err(|_| Error::storage())?;
        let k = key(&row)?;
        event_tx(&mut tx, &AuditEvent::new("key_updated", "admin", None, None, serde_json::json!({"key_id":id,"name":name,"previous_group_id":previous,"group_id":group_id}))).await?;
        tx.commit().await.map_err(|_| Error::storage())?;
        Ok(k)
    }
}
