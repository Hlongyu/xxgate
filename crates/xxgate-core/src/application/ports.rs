use crate::{
    Result,
    access::{AdminSession, GatewayKey},
    accounts::{Account, DisableReason},
    audit::{AuditEvent, RequestRecord},
    groups::Group,
    identity::{Binding, IdMapping, SessionKey},
    pricing::Price,
    providers::ModelSpec,
    quota::QuotaWindow,
    reports::{RequestFilter, UsageFilter},
    settings::RuntimeSettings,
};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde_json::Value;
use uuid::Uuid;

#[async_trait]
pub trait AccountStore: Send + Sync {
    async fn delete_account(&self, id: Uuid) -> Result<()>;
    async fn accounts(&self) -> Result<Vec<Account>>;
    async fn put_account(
        &self,
        account: &Account,
        encrypted: Option<&[u8]>,
        expected_version: Option<i64>,
    ) -> Result<Account>;
    async fn credentials(&self, id: Uuid) -> Result<Vec<u8>>;
    async fn update_credentials(
        &self,
        id: Uuid,
        encrypted: &[u8],
        expires_at: Option<DateTime<Utc>>,
        expected_version: i64,
    ) -> Result<Account>;
    async fn set_enabled(
        &self,
        id: Uuid,
        enabled: bool,
        reason: Option<DisableReason>,
        expected_version: i64,
        actor: &str,
    ) -> Result<Account>;
    async fn save_quotas(&self, id: Uuid, windows: &[QuotaWindow]) -> Result<()>;
    async fn quotas(&self, id: Uuid) -> Result<Vec<QuotaWindow>>;
    async fn save_model_catalog(
        &self,
        id: Uuid,
        catalog: &crate::providers::AccountModelCatalog,
    ) -> Result<Account>;
}

#[async_trait]
pub trait ResetStore: Send + Sync {
    async fn reset_credits(&self, id: Uuid) -> Result<Option<crate::resets::ResetCredits>>;
    async fn save_reset_credits(
        &self,
        id: Uuid,
        credits: &crate::resets::ResetCredits,
    ) -> Result<()>;
    async fn reset_operation(
        &self,
        id: Uuid,
        operation: Option<Uuid>,
    ) -> Result<Option<crate::resets::ResetOperation>>;
    async fn begin_reset(&self, operation: &crate::resets::ResetOperation) -> Result<()>;
    async fn finish_reset(&self, operation: &crate::resets::ResetOperation) -> Result<()>;
}

#[async_trait]
pub trait IdentityStore: Send + Sync {
    async fn active_bindings(&self) -> Result<Vec<Binding>>;
    async fn commit_binding(&self, binding: &Binding, expected_generation: i64) -> Result<()>;
    async fn mappings(&self, binding_id: Uuid) -> Result<Vec<IdMapping>>;
    async fn save_mappings(&self, binding_id: Uuid, mappings: &[IdMapping]) -> Result<()>;
    async fn session_bindings(&self, session: &SessionKey) -> Result<Vec<Binding>>;
}

#[async_trait]
pub trait AccessStore: Send + Sync {
    async fn delete_key(&self, id: Uuid) -> Result<()>;
    async fn initialize_admin(&self, password_hash: &str) -> Result<bool>;
    async fn admin_password_hash(&self) -> Result<Option<String>>;
    async fn put_admin_session(&self, hash: &str, session: &AdminSession) -> Result<()>;
    async fn admin_session(&self, hash: &str) -> Result<Option<AdminSession>>;
    async fn delete_admin_session(&self, hash: &str) -> Result<()>;
    async fn keys(&self) -> Result<Vec<GatewayKey>>;
    async fn put_key(&self, key: &GatewayKey, hash: &str) -> Result<()>;
    async fn key_by_hash(&self, hash: &str) -> Result<Option<GatewayKey>>;
    async fn set_key_enabled(&self, id: Uuid, enabled: bool) -> Result<GatewayKey>;
    async fn update_key(&self, id: Uuid, name: &str, group_id: Uuid) -> Result<GatewayKey>;
}

#[async_trait]
pub trait GroupStore: Send + Sync {
    async fn groups(&self) -> Result<Vec<Group>>;
    async fn create_group(&self, name: &str) -> Result<Group>;
    async fn rename_group(&self, id: Uuid, name: &str) -> Result<Group>;
    async fn delete_group(&self, id: Uuid) -> Result<()>;
}

#[async_trait]
pub trait RequestStore: Send + Sync {
    async fn safety_rejection(&self, session: &SessionKey) -> Result<Option<crate::Error>>;
    async fn save_safety_rejection(&self, session: &SessionKey, error: &crate::Error)
    -> Result<()>;
    async fn begin_request(&self, record: &RequestRecord) -> Result<()>;
    async fn update_request(&self, record: &RequestRecord) -> Result<()>;
    async fn finish_request(&self, record: &RequestRecord) -> Result<()>;
    async fn append_event(&self, event: &AuditEvent) -> Result<()>;
    async fn reconcile_interrupted(&self) -> Result<u64>;
}

#[async_trait]
pub trait SettingsStore: Send + Sync {
    async fn search_price(&self) -> Result<crate::pricing::SearchPrice>;
    async fn put_search_price(
        &self,
        price: &crate::pricing::SearchPrice,
    ) -> Result<crate::pricing::SearchPrice>;
    async fn settings(&self) -> Result<RuntimeSettings>;
    async fn save_settings(
        &self,
        settings: &RuntimeSettings,
        expected_version: i64,
    ) -> Result<RuntimeSettings>;
    async fn models(&self) -> Result<Vec<ModelSpec>>;
    async fn put_model(&self, model: &ModelSpec) -> Result<ModelSpec>;
    async fn prices(&self) -> Result<Vec<Price>>;
    async fn put_price(&self, price: &Price) -> Result<Price>;
}

#[async_trait]
pub trait ReportStore: Send + Sync {
    async fn request_errors(&self, filter: &crate::reports::ErrorFilter) -> Result<Value>;
    async fn account_spending(
        &self,
        account_id: Uuid,
        now: DateTime<Utc>,
        quota_stale_seconds: u64,
    ) -> Result<Value>;
    async fn requests(&self, filter: &RequestFilter) -> Result<Value>;
    async fn request_detail(&self, id: Uuid) -> Result<Value>;
    async fn dashboard(&self, filter: &UsageFilter) -> Result<Value>;
    async fn audit_events(&self, offset: i64, limit: i64) -> Result<Value>;
    async fn cleanup(&self, settings: &RuntimeSettings) -> Result<Value>;
    async fn health(&self) -> Result<()>;
}

pub trait Store:
    AccountStore
    + ResetStore
    + IdentityStore
    + AccessStore
    + GroupStore
    + RequestStore
    + SettingsStore
    + ReportStore
{
}
impl<
    T: AccountStore
        + ResetStore
        + IdentityStore
        + AccessStore
        + GroupStore
        + RequestStore
        + SettingsStore
        + ReportStore,
> Store for T
{
}
