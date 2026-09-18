use super::accounts::{PgStore, event_tx};
use async_trait::async_trait;
use serde_json::json;
use sqlx::Row;
use uuid::Uuid;
use xxgate_core::{
    Error, Result,
    application::ports::GroupStore,
    audit::AuditEvent,
    groups::{DEFAULT_GROUP_ID, Group},
};

pub(crate) fn group_error(error: sqlx::Error) -> Error {
    match error.as_database_error().and_then(|e| e.code()).as_deref() {
        Some("23505") => Error::new(
            409,
            "group_name_exists",
            "A group with this name already exists",
        ),
        Some("23503") => Error::new(
            409,
            "group_in_use",
            "Move the group's keys and accounts before deleting it",
        ),
        _ => Error::storage(),
    }
}

pub(crate) async fn validate_groups(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    ids: &[Uuid],
) -> Result<()> {
    let found: Vec<Uuid> = sqlx::query_scalar(
        "SELECT id FROM routing_groups WHERE id=ANY($1) ORDER BY id FOR KEY SHARE",
    )
    .bind(ids)
    .fetch_all(&mut **tx)
    .await
    .map_err(|_| Error::storage())?;
    if ids.iter().any(|id| !found.contains(id)) {
        return Err(Error::new(
            400,
            "group_not_found",
            "One or more selected groups do not exist",
        ));
    }
    Ok(())
}

fn group(row: &sqlx::postgres::PgRow) -> Result<Group> {
    Ok(Group {
        id: row.try_get("id").map_err(|_| Error::storage())?,
        name: row.try_get("name").map_err(|_| Error::storage())?,
        created_at: row.try_get("created_at").map_err(|_| Error::storage())?,
        updated_at: row.try_get("updated_at").map_err(|_| Error::storage())?,
    })
}

#[async_trait]
impl GroupStore for PgStore {
    async fn groups(&self) -> Result<Vec<Group>> {
        sqlx::query("SELECT * FROM routing_groups ORDER BY created_at,id")
            .fetch_all(&self.pool)
            .await
            .map_err(|_| Error::storage())?
            .iter()
            .map(group)
            .collect()
    }
    async fn create_group(&self, name: &str) -> Result<Group> {
        Group::validate_name(name)?;
        let mut tx = self.pool.begin().await.map_err(|_| Error::storage())?;
        let row = sqlx::query("INSERT INTO routing_groups(id,name) VALUES($1,$2) RETURNING *")
            .bind(Uuid::new_v4())
            .bind(name.trim())
            .fetch_one(&mut *tx)
            .await
            .map_err(group_error)?;
        let g = group(&row)?;
        event_tx(
            &mut tx,
            &AuditEvent::new(
                "group_created",
                "admin",
                None,
                None,
                json!({"group_id":g.id,"name":g.name}),
            ),
        )
        .await?;
        tx.commit().await.map_err(|_| Error::storage())?;
        Ok(g)
    }
    async fn rename_group(&self, id: Uuid, name: &str) -> Result<Group> {
        Group::validate_name(name)?;
        let mut tx = self.pool.begin().await.map_err(|_| Error::storage())?;
        let row = sqlx::query(
            "UPDATE routing_groups SET name=$2,updated_at=now() WHERE id=$1 RETURNING *",
        )
        .bind(id)
        .bind(name.trim())
        .fetch_optional(&mut *tx)
        .await
        .map_err(group_error)?
        .ok_or_else(Error::not_found)?;
        let g = group(&row)?;
        event_tx(
            &mut tx,
            &AuditEvent::new(
                "group_updated",
                "admin",
                None,
                None,
                json!({"group_id":g.id,"name":g.name}),
            ),
        )
        .await?;
        tx.commit().await.map_err(|_| Error::storage())?;
        Ok(g)
    }
    async fn delete_group(&self, id: Uuid) -> Result<()> {
        if id == DEFAULT_GROUP_ID {
            return Err(Error::new(
                409,
                "default_group_required",
                "The default group cannot be deleted",
            ));
        }
        let mut tx = self.pool.begin().await.map_err(|_| Error::storage())?;
        let row = sqlx::query("DELETE FROM routing_groups WHERE id=$1 RETURNING *")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(group_error)?
            .ok_or_else(Error::not_found)?;
        let g = group(&row)?;
        event_tx(
            &mut tx,
            &AuditEvent::new(
                "group_deleted",
                "admin",
                None,
                None,
                json!({"group_id":g.id,"name":g.name}),
            ),
        )
        .await?;
        tx.commit().await.map_err(|_| Error::storage())?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use sqlx::Connection;

    #[tokio::test]
    #[ignore = "requires a disposable PostgreSQL database; run python3 scripts/test.py"]
    async fn migration_preserves_existing_keys_accounts_and_bindings() {
        let database = std::env::var("TEST_DATABASE_URL").unwrap();
        let mut connection = sqlx::PgConnection::connect(&database).await.unwrap();
        let mut tx = connection.begin().await.unwrap();
        let schema = format!("group_migration_{}", Uuid::new_v4().simple());
        sqlx::raw_sql(&format!(
            "CREATE SCHEMA {schema}; SET LOCAL search_path TO {schema};"
        ))
        .execute(&mut *tx)
        .await
        .unwrap();
        sqlx::raw_sql(include_str!("../migrations/0001_initial.sql"))
            .execute(&mut *tx)
            .await
            .unwrap();
        let account_id = Uuid::new_v4();
        let key_id = Uuid::new_v4();
        let binding_id = Uuid::new_v4();
        sqlx::query("INSERT INTO accounts(id,version,credential_version,enabled,data,credentials) VALUES($1,3,2,true,$2,decode('abcd','hex'))")
            .bind(account_id).bind(json!({"id":account_id,"name":"legacy-account","version":3})).execute(&mut *tx).await.unwrap();
        sqlx::query("INSERT INTO api_keys(id,name,prefix,secret_hash) VALUES($1,'legacy-key','sk-test','test-only-hash')")
            .bind(key_id).execute(&mut *tx).await.unwrap();
        sqlx::query("INSERT INTO bindings(id,key_id,client_session_id,generation,account_id,data) VALUES($1,$2,'session',4,$3,$4)")
            .bind(binding_id).bind(key_id).bind(account_id).bind(json!({"id":binding_id,"generation":4})).execute(&mut *tx).await.unwrap();
        sqlx::raw_sql(include_str!("../migrations/0005_groups.sql"))
            .execute(&mut *tx)
            .await
            .unwrap();
        let row = sqlx::query("SELECT group_id,secret_hash FROM api_keys WHERE id=$1")
            .bind(key_id)
            .fetch_one(&mut *tx)
            .await
            .unwrap();
        assert_eq!(row.get::<Uuid, _>("group_id"), DEFAULT_GROUP_ID);
        assert_eq!(row.get::<String, _>("secret_hash"), "test-only-hash");
        let row = sqlx::query("SELECT version,data,credentials FROM accounts WHERE id=$1")
            .bind(account_id)
            .fetch_one(&mut *tx)
            .await
            .unwrap();
        assert_eq!(row.get::<i64, _>("version"), 3);
        assert_eq!(row.get::<Vec<u8>, _>("credentials"), vec![0xab, 0xcd]);
        assert_eq!(
            row.get::<Value, _>("data")["group_ids"],
            json!([DEFAULT_GROUP_ID])
        );
        let membership: Uuid =
            sqlx::query_scalar("SELECT group_id FROM account_groups WHERE account_id=$1")
                .bind(account_id)
                .fetch_one(&mut *tx)
                .await
                .unwrap();
        assert_eq!(membership, DEFAULT_GROUP_ID);
        let binding: Value = sqlx::query_scalar("SELECT data FROM bindings WHERE id=$1")
            .bind(binding_id)
            .fetch_one(&mut *tx)
            .await
            .unwrap();
        assert_eq!(binding["generation"], 4);
        assert_eq!(binding["group_id"], json!(DEFAULT_GROUP_ID));
        tx.rollback().await.unwrap();
    }
}
