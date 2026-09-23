use crate::PgStore;
use xxgate_core::{Error, Result};

impl PgStore {
    pub async fn model_discovery_version(&self) -> Result<Option<String>> {
        sqlx::query_scalar("SELECT version FROM model_discovery_version WHERE singleton=TRUE")
            .fetch_optional(&self.pool)
            .await
            .map_err(|_| Error::storage())
    }
    pub async fn save_model_discovery_version(&self, version: &str) -> Result<()> {
        sqlx::query("INSERT INTO model_discovery_version(singleton,version) VALUES(TRUE,$1) ON CONFLICT(singleton) DO UPDATE SET version=EXCLUDED.version,synced_at=now()")
            .bind(version).execute(&self.pool).await.map_err(|_| Error::storage())?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    #[ignore = "requires TEST_DATABASE_URL"]
    async fn discovery_version_survives_reconnection() {
        let url = std::env::var("TEST_DATABASE_URL").unwrap();
        let store = PgStore::connect(&url).await.unwrap();
        store.save_model_discovery_version("0.155.1").await.unwrap();
        let reconnected = PgStore::connect(&url).await.unwrap();
        assert_eq!(
            reconnected
                .model_discovery_version()
                .await
                .unwrap()
                .as_deref(),
            Some("0.155.1")
        );
        reconnected
            .save_model_discovery_version("0.156.0")
            .await
            .unwrap();
        assert_eq!(
            store.model_discovery_version().await.unwrap().as_deref(),
            Some("0.156.0")
        );
    }
}
