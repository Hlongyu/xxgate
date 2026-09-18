use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use sqlx::{PgPool, Row, postgres::PgPoolOptions};
use uuid::Uuid;
use xxgate_core::{
    Error, Result,
    accounts::{Account, DisableReason},
    application::ports::AccountStore,
    audit::AuditEvent,
    quota::QuotaWindow,
};

#[derive(Clone)]
pub struct PgStore {
    pub(crate) pool: PgPool,
}
impl PgStore {
    pub async fn connect(url: &str) -> Result<Self> {
        let pool = PgPoolOptions::new()
            .max_connections(24)
            .acquire_timeout(std::time::Duration::from_secs(5))
            .connect(url)
            .await
            .map_err(|_| Error::storage())?;
        sqlx::migrate!("./migrations")
            .run(&pool)
            .await
            .map_err(|_| Error::new(503, "migration_failed", "Database migrations failed"))?;
        Ok(Self { pool })
    }
    pub async fn single_instance_lock(&self) -> Result<sqlx::pool::PoolConnection<sqlx::Postgres>> {
        let mut connection = self.pool.acquire().await.map_err(|_| Error::storage())?;
        let locked: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock(848847415, 1)")
            .fetch_one(&mut *connection)
            .await
            .map_err(|_| Error::storage())?;
        if !locked {
            return Err(Error::new(
                503,
                "instance_already_running",
                "Another XXGate instance is using this database",
            ));
        }
        Ok(connection)
    }
}

pub(crate) fn decode<T: serde::de::DeserializeOwned>(row: &sqlx::postgres::PgRow) -> Result<T> {
    serde_json::from_value(
        row.try_get::<Value, _>("data")
            .map_err(|_| Error::storage())?,
    )
    .map_err(|_| Error::storage())
}
pub(crate) fn encode<T: serde::Serialize>(value: &T) -> Result<Value> {
    serde_json::to_value(value).map_err(|_| Error::storage())
}
pub(crate) async fn event_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    event: &AuditEvent,
) -> Result<()> {
    sqlx::query("INSERT INTO audit_events(id,request_id,account_id,kind,actor,at,data) VALUES($1,$2,$3,$4,$5,$6,$7) ON CONFLICT(id) DO NOTHING")
        .bind(event.id).bind(event.request_id).bind(event.account_id).bind(&event.kind).bind(&event.actor).bind(event.at).bind(encode(event)?)
        .execute(&mut **tx).await.map_err(|_| Error::storage())?;
    Ok(())
}

#[async_trait]
impl AccountStore for PgStore {
    async fn save_model_catalog(
        &self,
        id: Uuid,
        catalog: &xxgate_core::providers::AccountModelCatalog,
    ) -> Result<Account> {
        let mut tx = self.pool.begin().await.map_err(|_| Error::storage())?;
        let row = sqlx::query("SELECT data FROM accounts WHERE id=$1 FOR UPDATE")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|_| Error::storage())?
            .ok_or_else(Error::not_found)?;
        let mut account: Account = decode(&row)?;
        account.model_catalog = Some(catalog.clone());
        sqlx::query("UPDATE accounts SET data=$2 WHERE id=$1")
            .bind(id)
            .bind(encode(&account)?)
            .execute(&mut *tx)
            .await
            .map_err(|_| Error::storage())?;
        tx.commit().await.map_err(|_| Error::storage())?;
        Ok(account)
    }
    async fn accounts(&self) -> Result<Vec<Account>> {
        sqlx::query("SELECT data FROM accounts ORDER BY updated_at DESC")
            .fetch_all(&self.pool)
            .await
            .map_err(|_| Error::storage())?
            .iter()
            .map(decode)
            .collect()
    }
    async fn put_account(
        &self,
        account: &Account,
        encrypted: Option<&[u8]>,
        expected_version: Option<i64>,
    ) -> Result<Account> {
        let mut tx = self.pool.begin().await.map_err(|_| Error::storage())?;
        super::groups::validate_groups(&mut tx, &account.group_ids).await?;
        let mut account = account.clone();
        account.updated_at = Utc::now();
        account.version = expected_version.unwrap_or(0) + 1;
        if let Some(expected) = expected_version {
            let row = sqlx::query("SELECT data FROM accounts WHERE id=$1 FOR UPDATE")
                .bind(account.id)
                .fetch_optional(&mut *tx)
                .await
                .map_err(|_| Error::storage())?
                .ok_or_else(Error::not_found)?;
            let previous: Account = decode(&row)?;
            if previous.version != expected {
                return Err(Error::conflict());
            }
            account.created_at = previous.created_at;
            account.credential_version = previous.credential_version;
            if encrypted.is_some() {
                account.credential_version += 1;
            }
            sqlx::query("UPDATE accounts SET version=$2,credential_version=$3,enabled=$4,data=$5,credentials=COALESCE($6,credentials),updated_at=now() WHERE id=$1")
                .bind(account.id).bind(account.version).bind(account.credential_version).bind(account.enabled).bind(encode(&account)?).bind(encrypted).execute(&mut *tx).await.map_err(|_| Error::storage())?;
        } else {
            account.credential_version = 1;
            sqlx::query("INSERT INTO accounts(id,version,credential_version,enabled,data,credentials) VALUES($1,$2,$3,$4,$5,$6)")
                .bind(account.id).bind(account.version).bind(account.credential_version).bind(account.enabled).bind(encode(&account)?).bind(encrypted.ok_or_else(|| Error::invalid("Credentials are required"))?).execute(&mut *tx).await.map_err(|_| Error::storage())?;
        }
        sqlx::query("DELETE FROM account_groups WHERE account_id=$1")
            .bind(account.id)
            .execute(&mut *tx)
            .await
            .map_err(|_| Error::storage())?;
        for group_id in &account.group_ids {
            sqlx::query("INSERT INTO account_groups(account_id,group_id) VALUES($1,$2)")
                .bind(account.id)
                .bind(group_id)
                .execute(&mut *tx)
                .await
                .map_err(|_| Error::storage())?;
        }
        event_tx(&mut tx, &AuditEvent::new(if expected_version.is_some() { "account_updated" } else { "account_created" }, "admin", None, Some(account.id), json!({"version":account.version,"name":account.name,"enabled":account.enabled,"codex_only":account.codex_only,"max_inflight":account.max_inflight,"group_ids":account.group_ids}))).await?;
        tx.commit().await.map_err(|_| Error::storage())?;
        Ok(account)
    }
    async fn credentials(&self, id: Uuid) -> Result<Vec<u8>> {
        sqlx::query_scalar("SELECT credentials FROM accounts WHERE id=$1")
            .bind(id)
            .fetch_optional(&self.pool)
            .await
            .map_err(|_| Error::storage())?
            .ok_or_else(Error::not_found)
    }
    async fn update_credentials(
        &self,
        id: Uuid,
        encrypted: &[u8],
        expires_at: Option<DateTime<Utc>>,
        expected_version: i64,
    ) -> Result<Account> {
        let mut tx = self.pool.begin().await.map_err(|_| Error::storage())?;
        let row = sqlx::query("SELECT data FROM accounts WHERE id=$1 FOR UPDATE")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|_| Error::storage())?
            .ok_or_else(Error::not_found)?;
        let mut a: Account = decode(&row)?;
        if a.credential_version != expected_version {
            return Err(Error::conflict());
        }
        a.credential_version += 1;
        a.credential_expires_at = expires_at;
        a.updated_at = Utc::now();
        sqlx::query("UPDATE accounts SET credential_version=$2,credentials=$3,data=$4,updated_at=now() WHERE id=$1").bind(id).bind(a.credential_version).bind(encrypted).bind(encode(&a)?).execute(&mut *tx).await.map_err(|_| Error::storage())?;
        event_tx(&mut tx, &AuditEvent::new("credentials_updated","oauth",None,Some(id),json!({"credential_version":a.credential_version,"enabled":a.enabled,"expires_at":expires_at}))).await?;
        tx.commit().await.map_err(|_| Error::storage())?;
        Ok(a)
    }
    async fn set_enabled(
        &self,
        id: Uuid,
        enabled: bool,
        reason: Option<DisableReason>,
        expected_version: i64,
        actor: &str,
    ) -> Result<Account> {
        let mut tx = self.pool.begin().await.map_err(|_| Error::storage())?;
        let row = sqlx::query("SELECT data FROM accounts WHERE id=$1 FOR UPDATE")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|_| Error::storage())?
            .ok_or_else(Error::not_found)?;
        let mut a: Account = decode(&row)?;
        if a.version != expected_version {
            return Err(Error::conflict());
        }
        a.version += 1;
        a.enabled = enabled;
        a.disable_reason = if enabled { None } else { reason };
        a.updated_at = Utc::now();
        sqlx::query(
            "UPDATE accounts SET version=$2,enabled=$3,data=$4,updated_at=now() WHERE id=$1",
        )
        .bind(id)
        .bind(a.version)
        .bind(enabled)
        .bind(encode(&a)?)
        .execute(&mut *tx)
        .await
        .map_err(|_| Error::storage())?;
        event_tx(
            &mut tx,
            &AuditEvent::new(
                if enabled {
                    "account_enabled"
                } else {
                    "account_disabled"
                },
                actor,
                None,
                Some(id),
                json!({"reason":a.disable_reason,"version":a.version}),
            ),
        )
        .await?;
        tx.commit().await.map_err(|_| Error::storage())?;
        Ok(a)
    }
    async fn save_quotas(&self, id: Uuid, windows: &[QuotaWindow]) -> Result<()> {
        let mut tx = self.pool.begin().await.map_err(|_| Error::storage())?;
        for w in windows {
            sqlx::query("INSERT INTO quota_snapshots(account_id,pool,window_minutes,observed_at,data) VALUES($1,$2,$3,$4,$5)").bind(id).bind(&w.pool).bind(w.window_minutes.unwrap_or(-1)).bind(w.observed_at).bind(encode(w)?).execute(&mut *tx).await.map_err(|_| Error::storage())?;
        }
        tx.commit().await.map_err(|_| Error::storage())?;
        Ok(())
    }
    async fn quotas(&self, id: Uuid) -> Result<Vec<QuotaWindow>> {
        sqlx::query("SELECT DISTINCT ON(pool,window_minutes) data FROM quota_snapshots WHERE account_id=$1 ORDER BY pool,window_minutes,observed_at DESC,id DESC").bind(id).fetch_all(&self.pool).await.map_err(|_| Error::storage())?.iter().map(decode).collect()
    }
}
