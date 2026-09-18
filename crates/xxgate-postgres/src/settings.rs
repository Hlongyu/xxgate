use super::accounts::{PgStore, decode, encode, event_tx};
use async_trait::async_trait;
use serde_json::json;
use sqlx::Row;
use xxgate_core::{
    Error, Result, application::ports::SettingsStore, audit::AuditEvent, pricing::Price,
    providers::ModelSpec, settings::RuntimeSettings,
};

#[async_trait]
impl SettingsStore for PgStore {
    async fn search_price(&self) -> Result<xxgate_core::pricing::SearchPrice> {
        sqlx::query("SELECT data FROM search_prices ORDER BY version DESC LIMIT 1")
            .fetch_optional(&self.pool)
            .await
            .map_err(|_| Error::storage())?
            .as_ref()
            .map(decode)
            .transpose()
            .map(|p| p.unwrap_or_default())
    }
    async fn put_search_price(
        &self,
        price: &xxgate_core::pricing::SearchPrice,
    ) -> Result<xxgate_core::pricing::SearchPrice> {
        price.validate()?;
        let mut tx = self.pool.begin().await.map_err(|_| Error::storage())?;
        sqlx::query("SELECT pg_advisory_xact_lock(848847415, 2)")
            .execute(&mut *tx)
            .await
            .map_err(|_| Error::storage())?;
        let previous: Option<i64> = sqlx::query_scalar("SELECT max(version) FROM search_prices")
            .fetch_one(&mut *tx)
            .await
            .map_err(|_| Error::storage())?;
        if previous.unwrap_or(0) != price.version {
            return Err(Error::conflict());
        }
        let mut next = price.clone();
        next.version =
            sqlx::query_scalar("INSERT INTO search_prices(data) VALUES($1) RETURNING version")
                .bind(encode(price)?)
                .fetch_one(&mut *tx)
                .await
                .map_err(|_| Error::storage())?;
        sqlx::query("UPDATE search_prices SET data=$2 WHERE version=$1")
            .bind(next.version)
            .bind(encode(&next)?)
            .execute(&mut *tx)
            .await
            .map_err(|_| Error::storage())?;
        event_tx(
            &mut tx,
            &AuditEvent::new(
                "search_price_updated",
                "admin",
                None,
                None,
                json!({"price":next}),
            ),
        )
        .await?;
        tx.commit().await.map_err(|_| Error::storage())?;
        Ok(next)
    }
    async fn settings(&self) -> Result<RuntimeSettings> {
        sqlx::query(
            "INSERT INTO settings(singleton,version,data) VALUES(TRUE,1,$1) ON CONFLICT DO NOTHING",
        )
        .bind(encode(&RuntimeSettings::default())?)
        .execute(&self.pool)
        .await
        .map_err(|_| Error::storage())?;
        decode(
            &sqlx::query("SELECT data FROM settings WHERE singleton=TRUE")
                .fetch_one(&self.pool)
                .await
                .map_err(|_| Error::storage())?,
        )
    }
    async fn save_settings(
        &self,
        settings: &RuntimeSettings,
        expected_version: i64,
    ) -> Result<RuntimeSettings> {
        settings.validate()?;
        let mut next = settings.clone();
        next.version = expected_version + 1;
        let mut tx = self.pool.begin().await.map_err(|_| Error::storage())?;
        let row = sqlx::query("SELECT version,data FROM settings WHERE singleton=TRUE FOR UPDATE")
            .fetch_one(&mut *tx)
            .await
            .map_err(|_| Error::storage())?;
        if row
            .try_get::<i64, _>("version")
            .map_err(|_| Error::storage())?
            != expected_version
        {
            return Err(Error::conflict());
        }
        let previous: RuntimeSettings = decode(&row)?;
        sqlx::query("UPDATE settings SET version=$1,data=$2 WHERE singleton=TRUE")
            .bind(next.version)
            .bind(encode(&next)?)
            .execute(&mut *tx)
            .await
            .map_err(|_| Error::storage())?;
        event_tx(
            &mut tx,
            &AuditEvent::new(
                "settings_changed",
                "admin",
                None,
                None,
                json!({"before":previous,"after":next}),
            ),
        )
        .await?;
        tx.commit().await.map_err(|_| Error::storage())?;
        Ok(next)
    }
    async fn models(&self) -> Result<Vec<ModelSpec>> {
        sqlx::query("SELECT data FROM model_catalog ORDER BY id")
            .fetch_all(&self.pool)
            .await
            .map_err(|_| Error::storage())?
            .iter()
            .map(decode)
            .collect()
    }
    async fn put_model(&self, model: &ModelSpec) -> Result<ModelSpec> {
        model.validate()?;
        let mut m = model.clone();
        let mut tx = self.pool.begin().await.map_err(|_| Error::storage())?;
        let version: Option<i64> =
            sqlx::query_scalar("SELECT version FROM model_catalog WHERE id=$1 FOR UPDATE")
                .bind(&m.id)
                .fetch_optional(&mut *tx)
                .await
                .map_err(|_| Error::storage())?;
        if version.unwrap_or(0) != m.version {
            return Err(Error::conflict());
        }
        m.version += 1;
        sqlx::query("INSERT INTO model_catalog(id,version,data) VALUES($1,$2,$3) ON CONFLICT(id) DO UPDATE SET version=EXCLUDED.version,data=EXCLUDED.data").bind(&m.id).bind(m.version).bind(encode(&m)?).execute(&mut *tx).await.map_err(|_|Error::storage())?;
        event_tx(
            &mut tx,
            &AuditEvent::new("model_updated", "admin", None, None, json!({"model":m})),
        )
        .await?;
        tx.commit().await.map_err(|_| Error::storage())?;
        Ok(m)
    }
    async fn prices(&self) -> Result<Vec<Price>> {
        sqlx::query("SELECT DISTINCT ON(provider,access_kind,model) data FROM prices ORDER BY provider,access_kind,model,version DESC").fetch_all(&self.pool).await.map_err(|_|Error::storage())?.iter().map(decode).collect()
    }
    async fn put_price(&self, price: &Price) -> Result<Price> {
        price.validate()?;
        let mut tx = self.pool.begin().await.map_err(|_| Error::storage())?;
        let mut price = price.clone();
        price.version=sqlx::query_scalar("INSERT INTO prices(provider,access_kind,model,data) VALUES($1,$2,$3,$4) RETURNING version").bind(&price.model.provider).bind(&price.model.access_kind).bind(&price.model.model).bind(encode(&price)?).fetch_one(&mut *tx).await.map_err(|_|Error::storage())?;
        sqlx::query("UPDATE prices SET data=$2 WHERE version=$1")
            .bind(price.version)
            .bind(encode(&price)?)
            .execute(&mut *tx)
            .await
            .map_err(|_| Error::storage())?;
        event_tx(
            &mut tx,
            &AuditEvent::new("price_updated", "admin", None, None, json!({"price":price})),
        )
        .await?;
        tx.commit().await.map_err(|_| Error::storage())?;
        Ok(price)
    }
}
