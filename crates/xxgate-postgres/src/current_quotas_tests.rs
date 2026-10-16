use crate::PgStore;
use chrono::{Duration, Utc};
use serde_json::json;
use sqlx::{PgPool, postgres::PgPoolOptions};
use uuid::Uuid;
use xxgate_core::{application::ports::AccountStore, quota::QuotaWindow};

#[tokio::test]
#[ignore = "requires disposable PostgreSQL"]
async fn current_quotas_backfill_ordering_atomicity_and_history_cleanup() {
    let url = std::env::var("TEST_DATABASE_URL").unwrap();
    let schema = format!("current_quota_test_{}", Uuid::new_v4().simple());
    let admin = PgPool::connect(&url).await.unwrap();
    sqlx::raw_sql(&format!("CREATE SCHEMA {schema}"))
        .execute(&admin)
        .await
        .unwrap();
    let path = schema.clone();
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .after_connect(move |conn, _| {
            let path = path.clone();
            Box::pin(async move {
                sqlx::query("SELECT set_config('search_path',$1,false)")
                    .bind(path)
                    .execute(conn)
                    .await?;
                Ok(())
            })
        })
        .connect(&url)
        .await
        .unwrap();
    sqlx::raw_sql("CREATE TABLE quota_snapshots (id BIGSERIAL PRIMARY KEY,account_id UUID NOT NULL,pool TEXT NOT NULL,window_minutes BIGINT NOT NULL,observed_at TIMESTAMPTZ NOT NULL,data JSONB NOT NULL)")
        .execute(&pool).await.unwrap();
    let store = PgStore { pool };
    let account = Uuid::new_v4();
    let other = Uuid::new_v4();
    let now = Utc::now();
    let window = QuotaWindow {
        pool: "codex".into(),
        window_minutes: Some(300),
        used_percent: 100.0,
        resets_at: Some(now + Duration::hours(1)),
        observed_at: now,
        source: "test".into(),
    };
    store
        .save_quotas(account, std::slice::from_ref(&window))
        .await
        .unwrap();
    let mut older = window.clone();
    older.observed_at -= Duration::seconds(1);
    older.used_percent = 10.0;
    store
        .save_quotas(account, std::slice::from_ref(&older))
        .await
        .unwrap();
    // A later ID with an older observation must not win the initial backfill.
    sqlx::raw_sql(include_str!("../migrations/0018_current_quotas.sql"))
        .execute(&store.pool)
        .await
        .unwrap();
    assert_eq!(store.quotas(account).await.unwrap()[0].used_percent, 100.0);
    assert!(store.quotas(other).await.unwrap().is_empty());
    let mut recovered = window.clone();
    recovered.observed_at += Duration::seconds(1);
    recovered.used_percent = 0.0;
    // Concurrent out-of-order observations converge on the newest timestamp.
    let (a, b) = tokio::join!(
        store.save_quotas(account, std::slice::from_ref(&recovered)),
        store.save_quotas(account, std::slice::from_ref(&older))
    );
    a.unwrap();
    b.unwrap();
    assert_eq!(store.quotas(account).await.unwrap()[0].used_percent, 0.0);
    recovered.used_percent = 7.0;
    store
        .save_quotas(account, std::slice::from_ref(&recovered))
        .await
        .unwrap();
    assert_eq!(store.quotas(account).await.unwrap()[0].used_percent, 7.0);
    let mut weekly = window.clone();
    weekly.window_minutes = Some(10080);
    store
        .save_quotas(account, std::slice::from_ref(&weekly))
        .await
        .unwrap();
    store.save_quotas(other, &[older]).await.unwrap();
    assert_eq!(store.quotas(account).await.unwrap().len(), 2);
    assert_eq!(store.quotas(other).await.unwrap()[0].used_percent, 10.0);
    // A rolled-back historical write cannot leak into current state.
    let mut tx = store.pool.begin().await.unwrap();
    recovered.observed_at += Duration::hours(1);
    recovered.used_percent = 99.0;
    sqlx::query("INSERT INTO quota_snapshots(account_id,pool,window_minutes,observed_at,data) VALUES($1,'codex',300,$2,$3)")
        .bind(account).bind(recovered.observed_at).bind(json!(recovered)).execute(&mut *tx).await.unwrap();
    tx.rollback().await.unwrap();
    assert_eq!(store.quotas(account).await.unwrap()[0].used_percent, 7.0);
    // History retention never removes the authoritative current state.
    sqlx::query("DELETE FROM quota_snapshots")
        .execute(&store.pool)
        .await
        .unwrap();
    assert_eq!(store.quotas(account).await.unwrap()[0].used_percent, 7.0);
    sqlx::raw_sql(&format!("DROP SCHEMA {schema} CASCADE"))
        .execute(&admin)
        .await
        .unwrap();
    store.pool.close().await;
    admin.close().await;
}
