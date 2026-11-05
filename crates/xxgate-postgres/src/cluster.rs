use crate::{PgStore, accounts::decode};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::Row;
use uuid::Uuid;
use xxgate_core::{Error, Result, accounts::Account, identity::SessionKey};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Node {
    pub id: String,
    pub role: String,
    pub url: String,
    pub boot_id: Uuid,
    pub heartbeat_at: DateTime<Utc>,
    pub accepting: bool,
    pub online: bool,
    pub runtime: Value,
}

impl PgStore {
    pub fn for_node(&self, id: &str) -> Result<Self> {
        if id.is_empty()
            || id.len() > 64
            || !id
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"_-".contains(&c))
        {
            return Err(Error::invalid(
                "Node ID must contain 1–64 ASCII letters, digits, underscores or hyphens",
            ));
        }
        Ok(Self {
            pool: self.pool.clone(),
            node_id: id.into(),
        })
    }

    pub fn node_id(&self) -> &str {
        &self.node_id
    }

    pub async fn node_accounts(&self) -> Result<Vec<(String, Account)>> {
        sqlx::query("SELECT node_id,data FROM accounts WHERE deleted_at IS NULL")
            .fetch_all(&self.pool)
            .await
            .map_err(|_| Error::storage())?
            .iter()
            .map(|r| {
                Ok((
                    r.try_get("node_id").map_err(|_| Error::storage())?,
                    decode(r)?,
                ))
            })
            .collect()
    }
    pub async fn all_accounts(&self) -> Result<Vec<Account>> {
        sqlx::query("SELECT data FROM accounts WHERE deleted_at IS NULL ORDER BY updated_at DESC")
            .fetch_all(&self.pool)
            .await
            .map_err(|_| Error::storage())?
            .iter()
            .map(decode)
            .collect()
    }

    pub async fn account_node(&self, id: Uuid) -> Result<String> {
        sqlx::query_scalar("SELECT node_id FROM accounts WHERE id=$1 AND deleted_at IS NULL")
            .bind(id)
            .fetch_optional(&self.pool)
            .await
            .map_err(|_| Error::storage())?
            .ok_or_else(Error::not_found)
    }

    pub(crate) async fn require_account_owner(&self, id: Uuid) -> Result<()> {
        if self.account_node(id).await? != self.node_id {
            return Err(Error::new(
                409,
                "account_node_mismatch",
                "The account belongs to another node",
            ));
        }
        Ok(())
    }

    pub async fn register_node(&self, role: &str, url: &str, boot: Uuid) -> Result<()> {
        sqlx::query("INSERT INTO cluster_nodes(id,role,url,boot_id) VALUES($1,$2,$3,$4) ON CONFLICT(id) DO UPDATE SET role=EXCLUDED.role,url=EXCLUDED.url,boot_id=EXCLUDED.boot_id,heartbeat_at=now(),accepting=false,runtime='{}'")
            .bind(&self.node_id).bind(role).bind(url).bind(boot).execute(&self.pool).await.map_err(|_| Error::storage())?;
        Ok(())
    }

    pub async fn heartbeat(&self, boot: Uuid, accepting: bool, runtime: Value) -> Result<()> {
        let changed = sqlx::query("UPDATE cluster_nodes SET heartbeat_at=now(),accepting=$3,runtime=$4 WHERE id=$1 AND boot_id=$2")
            .bind(&self.node_id).bind(boot).bind(accepting).bind(runtime).execute(&self.pool).await.map_err(|_| Error::storage())?.rows_affected();
        if changed != 1 {
            return Err(Error::new(
                503,
                "node_ownership_lost",
                "Node ownership was lost",
            ));
        }
        Ok(())
    }

    pub async fn nodes(&self) -> Result<Vec<Node>> {
        sqlx::query("SELECT *, heartbeat_at>now()-interval '10 seconds' AS online FROM cluster_nodes ORDER BY id")
            .fetch_all(&self.pool).await.map_err(|_| Error::storage())?.iter().map(|r| {
                Ok(Node {
                    id:r.try_get("id").map_err(|_| Error::storage())?,
                    role:r.try_get("role").map_err(|_| Error::storage())?,
                    url:r.try_get("url").map_err(|_| Error::storage())?,
                    boot_id:r.try_get("boot_id").map_err(|_| Error::storage())?,
                    heartbeat_at:r.try_get("heartbeat_at").map_err(|_| Error::storage())?,
                    accepting:r.try_get("accepting").map_err(|_| Error::storage())?,
                    online:r.try_get("online").map_err(|_| Error::storage())?,
                    runtime:r.try_get("runtime").map_err(|_| Error::storage())?,
                })
            }).collect()
    }

    pub async fn session_node(&self, session: &SessionKey) -> Result<Option<String>> {
        sqlx::query_scalar(
            "SELECT node_id FROM session_nodes WHERE key_id=$1 AND client_session_id=$2",
        )
        .bind(session.key_id)
        .bind(&session.client_session_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|_| Error::storage())
    }

    /// Concurrent first requests must observe the same winner, including across routers.
    pub async fn claim_session(&self, session: &SessionKey, node: &str) -> Result<String> {
        sqlx::query("INSERT INTO session_nodes(key_id,client_session_id,node_id) VALUES($1,$2,$3) ON CONFLICT DO NOTHING")
            .bind(session.key_id).bind(&session.client_session_id).bind(node).execute(&self.pool).await.map_err(|_| Error::storage())?;
        self.session_node(session).await?.ok_or_else(Error::storage)
    }

    pub async fn put_operation(&self, id: Uuid) -> Result<()> {
        sqlx::query("INSERT INTO cluster_operations(id,node_id) VALUES($1,$2)")
            .bind(id)
            .bind(&self.node_id)
            .execute(&self.pool)
            .await
            .map_err(|_| Error::storage())?;
        Ok(())
    }

    pub async fn operation_node(&self, id: Uuid) -> Result<String> {
        sqlx::query_scalar(
            "SELECT node_id FROM cluster_operations WHERE id=$1 AND expires_at>now()",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|_| Error::storage())?
        .ok_or_else(Error::not_found)
    }

    pub async fn revision(&self) -> Result<i64> {
        sqlx::query_scalar("SELECT version FROM cluster_revision WHERE singleton=true")
            .fetch_one(&self.pool)
            .await
            .map_err(|_| Error::storage())
    }

    /// One cleanup execution at a time, without holding a transaction for its work.
    pub async fn maintenance_lock(&self) -> Result<Option<sqlx::PgConnection>> {
        let mut conn = self.pool.acquire().await.map_err(|_| Error::storage())?;
        let locked: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock(848847415, 3)")
            .fetch_one(&mut *conn)
            .await
            .map_err(|_| Error::storage())?;
        if locked {
            Ok(Some(conn.detach()))
        } else {
            Ok(None)
        }
    }
}
