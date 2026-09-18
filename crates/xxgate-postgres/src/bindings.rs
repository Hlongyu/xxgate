use super::accounts::{PgStore, decode, encode, event_tx};
use async_trait::async_trait;
use sqlx::Row;
use uuid::Uuid;
use xxgate_core::{
    Error, Result,
    application::ports::IdentityStore,
    audit::AuditEvent,
    identity::{Binding, IdMapping, SessionKey},
};

#[async_trait]
impl IdentityStore for PgStore {
    async fn active_bindings(&self) -> Result<Vec<Binding>> {
        sqlx::query("SELECT b.data FROM sessions s JOIN bindings b ON b.id=s.binding_id")
            .fetch_all(&self.pool)
            .await
            .map_err(|_| Error::storage())?
            .iter()
            .map(decode)
            .collect()
    }
    async fn commit_binding(&self, b: &Binding, expected_generation: i64) -> Result<()> {
        let mut tx = self.pool.begin().await.map_err(|_| Error::storage())?;
        sqlx::query(
            "INSERT INTO sessions(key_id,client_session_id) VALUES($1,$2) ON CONFLICT DO NOTHING",
        )
        .bind(b.session.key_id)
        .bind(&b.session.client_session_id)
        .execute(&mut *tx)
        .await
        .map_err(|_| Error::storage())?;
        let previous: i64 = sqlx::query_scalar(
            "SELECT generation FROM sessions WHERE key_id=$1 AND client_session_id=$2 FOR UPDATE",
        )
        .bind(b.session.key_id)
        .bind(&b.session.client_session_id)
        .fetch_one(&mut *tx)
        .await
        .map_err(|_| Error::storage())?;
        if previous != expected_generation || b.generation != previous + 1 {
            return Err(Error::conflict());
        }
        sqlx::query("INSERT INTO bindings(id,key_id,client_session_id,generation,account_id,data) VALUES($1,$2,$3,$4,$5,$6)").bind(b.id).bind(b.session.key_id).bind(&b.session.client_session_id).bind(b.generation).bind(b.account_id).bind(encode(b)?).execute(&mut *tx).await.map_err(|_| Error::storage())?;
        sqlx::query("UPDATE sessions SET generation=$3,binding_id=$4 WHERE key_id=$1 AND client_session_id=$2").bind(b.session.key_id).bind(&b.session.client_session_id).bind(b.generation).bind(b.id).execute(&mut *tx).await.map_err(|_| Error::storage())?;
        event_tx(&mut tx,&AuditEvent::new(if previous==0 {"session_bound"} else {"session_migrated"},"scheduler",None,Some(b.account_id),serde_json::json!({"binding_id":b.id,"session_id":b.session.client_session_id,"generation":b.generation,"key_id":b.session.key_id}))).await?;
        tx.commit().await.map_err(|_| Error::storage())?;
        Ok(())
    }
    async fn mappings(&self, id: Uuid) -> Result<Vec<IdMapping>> {
        sqlx::query("SELECT kind,client_id,upstream_id FROM identity_mappings WHERE binding_id=$1")
            .bind(id)
            .fetch_all(&self.pool)
            .await
            .map_err(|_| Error::storage())?
            .iter()
            .map(|r| {
                Ok(IdMapping {
                    kind: r.try_get("kind").map_err(|_| Error::storage())?,
                    client_id: r.try_get("client_id").map_err(|_| Error::storage())?,
                    upstream_id: r.try_get("upstream_id").map_err(|_| Error::storage())?,
                })
            })
            .collect()
    }
    async fn save_mappings(&self, id: Uuid, mappings: &[IdMapping]) -> Result<()> {
        if mappings.is_empty() {
            return Ok(());
        }
        let mut tx = self.pool.begin().await.map_err(|_| Error::storage())?;
        for m in mappings {
            let rows = sqlx::query("INSERT INTO identity_mappings(binding_id,kind,client_id,upstream_id) VALUES($1,$2,$3,$4) ON CONFLICT(binding_id,kind,client_id) DO UPDATE SET upstream_id=identity_mappings.upstream_id WHERE identity_mappings.upstream_id=EXCLUDED.upstream_id")
                .bind(id).bind(&m.kind).bind(&m.client_id).bind(&m.upstream_id).execute(&mut *tx).await.map_err(|_| Error::storage())?.rows_affected();
            if rows == 0 {
                return Err(Error::conflict());
            }
        }
        tx.commit().await.map_err(|_| Error::storage())?;
        Ok(())
    }
    async fn session_bindings(&self, s: &SessionKey) -> Result<Vec<Binding>> {
        sqlx::query("SELECT data FROM bindings WHERE key_id=$1 AND client_session_id=$2 ORDER BY generation").bind(s.key_id).bind(&s.client_session_id).fetch_all(&self.pool).await.map_err(|_| Error::storage())?.iter().map(decode).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    #[ignore = "requires a disposable PostgreSQL database; run python3 scripts/test.py"]
    async fn migration_repairs_legacy_tool_output_aliases_without_changing_other_mappings() {
        let database = std::env::var("TEST_DATABASE_URL").expect("TEST_DATABASE_URL is required");
        let store = PgStore::connect(&database).await.unwrap();
        let mut tx = store.pool.begin().await.unwrap();
        sqlx::query("CREATE TEMP TABLE identity_mappings (LIKE public.identity_mappings INCLUDING ALL) ON COMMIT DROP")
            .execute(&mut *tx).await.unwrap();
        let binding = Uuid::new_v4();
        let cases = [
            (
                "item",
                "ctco_client",
                "msg_11111111111111111111111111111111",
                "ctco_11111111111111111111111111111111",
            ),
            (
                "item",
                "fco_client",
                "msg_22222222222222222222222222222222",
                "fco_22222222222222222222222222222222",
            ),
            (
                "item",
                "future123_client",
                "msg_33333333333333333333333333333333",
                "future123_33333333333333333333333333333333",
            ),
            (
                "item",
                "msg_client",
                "msg_44444444444444444444444444444444",
                "msg_44444444444444444444444444444444",
            ),
            (
                "item",
                "msg_existing_alias",
                "ctco_upstream",
                "ctco_upstream",
            ),
            (
                "item",
                "ctco_correct",
                "ctco_already_correct",
                "ctco_already_correct",
            ),
            (
                "thread",
                "ctco_thread",
                "msg_55555555555555555555555555555555",
                "msg_55555555555555555555555555555555",
            ),
        ];
        for (kind, client, upstream, _) in cases {
            sqlx::query("INSERT INTO identity_mappings(binding_id,kind,client_id,upstream_id) VALUES($1,$2,$3,$4)")
                .bind(binding).bind(kind).bind(client).bind(upstream).execute(&mut *tx).await.unwrap();
        }
        for _ in 0..2 {
            sqlx::raw_sql(include_str!(
                "../migrations/0003_repair_item_id_prefixes.sql"
            ))
            .execute(&mut *tx)
            .await
            .unwrap();
            for (kind, client, _, expected) in cases {
                let actual: String = sqlx::query_scalar("SELECT upstream_id FROM identity_mappings WHERE binding_id=$1 AND kind=$2 AND client_id=$3")
                    .bind(binding).bind(kind).bind(client).fetch_one(&mut *tx).await.unwrap();
                assert_eq!(actual, expected);
            }
        }
        tx.rollback().await.unwrap();
    }
}
