use crate::PgStore;
use sqlx::{Connection, Row};
use xxgate_core::{Error, Result};

impl PgStore {
    /// Explicit maintenance action. Runtime nodes hold the same gate shared.
    pub async fn migrate(&self) -> Result<()> {
        let mut guard = self
            .pool
            .acquire()
            .await
            .map_err(|_| Error::storage())?
            .detach();
        sqlx::query("SET lock_timeout='5s'")
            .execute(&mut guard)
            .await
            .map_err(|_| Error::storage())?;
        sqlx::query("SELECT pg_advisory_lock(848847415,1)")
            .execute(&mut guard)
            .await
            .map_err(|_| {
                Error::new(
                    503,
                    "migration_requires_stop",
                    "Stop every XXGate process using this database before migrating",
                )
            })?;
        let result=sqlx::migrate!("./migrations").run(&self.pool).await
            .map_err(|_|Error::new(503,"migration_failed","Database migration failed; do not start an older image against the changed schema"));
        let _ = guard.close().await;
        result
    }

    /// Fail before traffic without creating a migration table or applying DDL.
    pub async fn check_schema(&self) -> Result<()> {
        let exists: bool = sqlx::query_scalar("SELECT to_regclass('_sqlx_migrations') IS NOT NULL")
            .fetch_one(&self.pool)
            .await
            .map_err(|_| Error::storage())?;
        let mismatch = || {
            Error::new(
                503,
                "schema_migration_required",
                "Database schema does not match this image; stop services and run xxgate-server migrate explicitly",
            )
        };
        if !exists {
            return Err(mismatch());
        }
        let rows =
            sqlx::query("SELECT version,success,checksum FROM _sqlx_migrations ORDER BY version")
                .fetch_all(&self.pool)
                .await
                .map_err(|_| Error::storage())?;
        let migrations = sqlx::migrate!("./migrations");
        if rows.len() != migrations.iter().count() {
            return Err(mismatch());
        }
        for (row, expected) in rows.iter().zip(migrations.iter()) {
            let version: i64 = row.try_get("version").map_err(|_| Error::storage())?;
            let success: bool = row.try_get("success").map_err(|_| Error::storage())?;
            let checksum: Vec<u8> = row.try_get("checksum").map_err(|_| Error::storage())?;
            if version != expected.version
                || !success
                || checksum.as_slice() != expected.checksum.as_ref()
            {
                return Err(mismatch());
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;
    #[tokio::test]
    #[ignore = "requires disposable PostgreSQL"]
    async fn runtime_is_read_only_and_migration_requires_stopped_nodes() {
        let database = std::env::var("TEST_DATABASE_URL").unwrap();
        let admin = sqlx::PgPool::connect(&database).await.unwrap();
        let schema = format!("migration_gate_{}", Uuid::new_v4().simple());
        // Advisory locks are database-wide, so isolate this deliberate 5s
        // lock-conflict test from other concurrently running migration tests.
        sqlx::query(&format!("CREATE DATABASE {schema}"))
            .execute(&admin)
            .await
            .unwrap();
        let url = format!("{}/{schema}", database.rsplit_once('/').unwrap().0);
        let store = PgStore::open(&url).await.unwrap();
        assert_eq!(
            store.check_schema().await.unwrap_err().code,
            "schema_migration_required"
        );
        let exists: bool = sqlx::query_scalar("SELECT to_regclass('_sqlx_migrations') IS NOT NULL")
            .fetch_one(&store.pool)
            .await
            .unwrap();
        assert!(!exists);
        // Simulate the lock held by the deployed legacy binary.
        let mut legacy = store.pool.acquire().await.unwrap().detach();
        sqlx::query("SELECT pg_advisory_lock(848847415,1)")
            .execute(&mut legacy)
            .await
            .unwrap();
        assert_eq!(
            store.migrate().await.unwrap_err().code,
            "migration_requires_stop"
        );
        assert!(store.single_instance_lock().await.is_err());
        legacy.close().await.unwrap();
        store.migrate().await.unwrap();
        store.check_schema().await.unwrap();
        let a = store.for_node("a").unwrap();
        let b = store.for_node("b").unwrap();
        let guard_a = a.single_instance_lock().await.unwrap();
        let guard_b = b.single_instance_lock().await.unwrap();
        assert!(a.single_instance_lock().await.is_err());
        assert_eq!(
            store.migrate().await.unwrap_err().code,
            "migration_requires_stop"
        );
        guard_a.close().await.unwrap();
        guard_b.close().await.unwrap();
        store.migrate().await.unwrap();
        store.pool.close().await;
        sqlx::query(&format!("DROP DATABASE {schema} WITH (FORCE)"))
            .execute(&admin)
            .await
            .unwrap();
        admin.close().await;
    }
}
