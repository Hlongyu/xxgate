use super::*;
use chrono::{Duration, Timelike};
use sqlx::{PgPool, Row, postgres::PgPoolOptions};
use xxgate_core::{
    application::ports::{ReportStore, RequestStore},
    audit::RequestRecord,
    settings::RuntimeSettings,
};

const MIGRATION: &str = include_str!("../migrations/0010_account_spending_rollups.sql");

async fn setup(migrate_rollups: bool) -> (PgStore, String) {
    let url = std::env::var("TEST_DATABASE_URL").unwrap();
    let schema = format!("spending_test_{}", Uuid::new_v4().simple());
    let admin = PgPool::connect(&url).await.unwrap();
    sqlx::raw_sql(&format!("CREATE SCHEMA {schema}"))
        .execute(&admin)
        .await
        .unwrap();
    admin.close().await;
    let search_path = schema.clone();
    let pool = PgPoolOptions::new()
        .max_connections(5)
        .after_connect(move |conn, _| {
            let search_path = search_path.clone();
            Box::pin(async move {
                sqlx::query("SELECT set_config('search_path',$1,false),set_config('TimeZone','Asia/Kathmandu',false)")
                    .bind(search_path).execute(conn).await?;
                Ok(())
            })
        })
        .connect(&url)
        .await
        .unwrap();
    for migration in sqlx::migrate!("./migrations").iter() {
        if migration.version < 10 || migrate_rollups {
            sqlx::raw_sql(&migration.sql).execute(&pool).await.unwrap();
        }
    }
    (PgStore { pool }, schema)
}

async fn teardown(store: PgStore, schema: String) {
    sqlx::raw_sql(&format!("DROP SCHEMA {schema} CASCADE"))
        .execute(&store.pool)
        .await
        .unwrap();
    store.pool.close().await;
}

fn record(account: Option<Uuid>, at: DateTime<Utc>, cny: Option<&str>) -> RequestRecord {
    serde_json::from_value(json!({
        "id":Uuid::new_v4(),"key_id":Uuid::new_v4(),"account_id":account,
        "client_session_id":"test","client_thread_id":"test","model":"m","provider":"openai",
        "state":"completed","created_at":at-Duration::minutes(1),"finished_at":at,
        "usage":xxgate_core::usage::Usage { complete:true, input_tokens:Some(1234), output_tokens:Some(56), ..Default::default() },
        "valuation":{"status":"priced","price_version":null,"cny":cny,"items":[]},
        "config_version":1,"config_versions":[1],"body_bytes":0,"upstream_attempts":1
    }))
    .unwrap()
}

async fn insert_historical(store: &PgStore, r: &RequestRecord) {
    sqlx::query("INSERT INTO requests(id,key_id,account_id,client_session_id,model,state,created_at,finished_at,data) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9)")
        .bind(r.id).bind(r.key_id).bind(r.account_id).bind(&r.client_session_id).bind(&r.model)
        .bind(&r.state).bind(r.created_at).bind(r.finished_at).bind(json!(r))
        .execute(&store.pool).await.unwrap();
}

async fn finish(store: &PgStore, r: &RequestRecord) {
    store.begin_request(r).await.unwrap();
    store.finish_request(r).await.unwrap();
}

async fn compare_windows(
    store: &PgStore,
    account: Uuid,
    now: DateTime<Utc>,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
) {
    let totals = sqlx::query_as::<_, SpendingTotal>(include_str!("account_spending.sql"))
        .bind(account)
        .bind(now)
        .bind(from)
        .bind(to)
        .bind(now - Duration::hours(5))
        .bind(now - Duration::days(7))
        .bind(now - Duration::days(30))
        .bind(from)
        .bind(to)
        .fetch_all(&store.pool)
        .await
        .unwrap();
    for t in totals {
        let (start, end, main) = match t.label.as_str() {
            "last_5h" => (now - Duration::hours(5), now, false),
            "last_7d" => (now - Duration::days(7), now, false),
            "last_30d" => (now - Duration::days(30), now, false),
            "weekly" | "monthly" => (from, to, true),
            _ => unreachable!(),
        };
        // The pre-migration query is the oracle, including its exact bounds,
        // frozen valuation, upstream-model fallback and Spark exclusion.
        let old = sqlx::query("SELECT COALESCE(sum((data->'valuation'->>'cny')::numeric),0)::text AS cny, count(*) AS requests, count(*) FILTER(WHERE data->'valuation'->>'cny' IS NULL) AS unpriced, sum((data->'usage'->>'input_tokens')::numeric)::text AS input_tokens, sum((data->'usage'->>'output_tokens')::numeric)::text AS output_tokens FROM requests WHERE account_id=$1 AND finished_at>$2 AND finished_at<=$3 AND (NOT $4 OR ((data->>'upstream_attempts')::bigint>0 AND COALESCE(data->>'upstream_model',model)<>'gpt-5.3-codex-spark'))")
            .bind(account).bind(start).bind(end).bind(main).fetch_one(&store.pool).await.unwrap();
        assert_eq!(
            t.cny.parse::<Decimal>().unwrap(),
            old.get::<String, _>("cny").parse::<Decimal>().unwrap(),
            "{} {start} {end}",
            t.label
        );
        if !main {
            for (actual, column) in [
                (&t.input_tokens, "input_tokens"),
                (&t.output_tokens, "output_tokens"),
            ] {
                let expected = if t.requests == 0 {
                    Some("0".to_owned())
                } else {
                    old.get::<Option<String>, _>(column)
                };
                assert_eq!(*actual, expected, "{} {column}", t.label);
            }
        }
        assert_eq!(
            t.requests,
            old.get::<i64, _>("requests"),
            "{} {start} {end}",
            t.label
        );
        assert_eq!(
            t.unpriced,
            old.get::<i64, _>("unpriced"),
            "{} {start} {end}",
            t.label
        );
    }
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database; run python3 scripts/test.py"]
async fn account_rollups_backfill_and_increment_match_exact_windows() {
    let (store, schema) = setup(false).await;
    let now = Utc::now().with_nanosecond(123_456_000).unwrap();
    let hour = now
        .with_minute(0)
        .unwrap()
        .with_second(0)
        .unwrap()
        .with_nanosecond(0)
        .unwrap();
    let id = Uuid::new_v4();
    let other = Uuid::new_v4();
    let mut records = Vec::new();
    // Both sides of rolling boundaries, exact UTC hours, subsecond bounds,
    // other accounts, missing/zero/fractional costs and rows ending in future.
    for boundary in [
        now,
        now - Duration::hours(5),
        now - Duration::days(7),
        now - Duration::days(30),
        hour,
        hour - Duration::hours(3),
    ] {
        for delta in [-1, 0, 1] {
            for cost in [None, Some("0"), Some("0.12345678")] {
                let mut r = record(Some(id), boundary + Duration::microseconds(delta), cost);
                r.created_at -= Duration::days(10); // Use completion, not creation.
                records.push(r);
            }
        }
    }
    for i in 0..200 {
        let mut r = record(
            Some(if i % 11 == 0 { other } else { id }),
            now - Duration::minutes(i * 51),
            Some("7.125"),
        );
        match i % 6 {
            0 => {
                r.upstream_model = Some("gpt-5.3-codex-spark".into());
                r.model = "alias".into();
            }
            1 => r.model = "gpt-5.3-codex-spark".into(),
            2 => r.upstream_attempts = 0,
            3 => r.kind = xxgate_core::protocol::RequestKind::Search,
            4 => {
                r.state = "cancelled".into();
                r.valuation = None;
            }
            _ => r.state = "failed".into(),
        }
        records.push(r);
    }
    records.push(record(None, now, Some("9000")));
    let mut inflight = record(Some(id), now, Some("100"));
    inflight.finished_at = None;
    insert_historical(&store, &inflight).await;
    for r in &records[..100] {
        insert_historical(&store, r).await;
    }
    sqlx::raw_sql(MIGRATION).execute(&store.pool).await.unwrap();
    sqlx::raw_sql(include_str!("../migrations/0011_account_cycle_tokens.sql"))
        .execute(&store.pool)
        .await
        .unwrap();
    sqlx::raw_sql(include_str!(
        "../migrations/0012_soft_delete_accounts_keys.sql"
    ))
    .execute(&store.pool)
    .await
    .unwrap();
    sqlx::raw_sql(include_str!(
        "../migrations/0013_monthly_account_spending.sql"
    ))
    .execute(&store.pool)
    .await
    .unwrap();
    // Replaying a previously finalized row cannot charge the backfill twice.
    store.finish_request(&records[0]).await.unwrap();
    for r in &records[100..] {
        finish(&store, r).await;
    }
    let bounds = [
        (hour - Duration::hours(3), hour),
        (
            hour + Duration::microseconds(1),
            hour + Duration::minutes(1),
        ),
        (hour, hour),
        (now - Duration::days(6), now - Duration::seconds(17)),
        (now - Duration::hours(5), now),
    ];
    for account in [id, other, Uuid::new_v4()] {
        for (from, to) in bounds {
            compare_windows(&store, account, now, from, to).await;
        }
    }
    inflight.finished_at = Some(now);
    store.finish_request(&inflight).await.unwrap();
    compare_windows(&store, id, now, bounds[0].0, bounds[0].1).await;
    let entries: i64 = sqlx::query_scalar("SELECT count(*) FROM account_spending_entries")
        .fetch_one(&store.pool)
        .await
        .unwrap();
    assert_eq!(entries, records.len() as i64); // One null account excluded; inflight added.
    teardown(store, schema).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database; run python3 scripts/test.py"]
async fn account_rollups_are_atomic_idempotent_and_survive_detail_cleanup() {
    let (store, schema) = setup(true).await;
    let now = Utc::now().with_nanosecond(0).unwrap();
    let id = Uuid::new_v4();
    let mut r = record(Some(id), now - Duration::days(2), Some("10"));
    store.begin_request(&r).await.unwrap();
    // Inject a failure after the request and original dashboard were updated.
    sqlx::raw_sql(
        "ALTER TABLE account_spending_hourly ADD CONSTRAINT test_failure CHECK (cny < 10)",
    )
    .execute(&store.pool)
    .await
    .unwrap();
    assert!(store.finish_request(&r).await.is_err());
    let state: bool = sqlx::query_scalar("SELECT finished_at IS NULL FROM requests WHERE id=$1")
        .bind(r.id)
        .fetch_one(&store.pool)
        .await
        .unwrap();
    assert!(state);
    for table in [
        "account_spending_entries",
        "account_spending_hourly",
        "request_hourly",
    ] {
        let count: i64 = sqlx::query_scalar(&format!("SELECT count(*) FROM {table}"))
            .fetch_one(&store.pool)
            .await
            .unwrap();
        assert_eq!(count, 0);
    }
    sqlx::raw_sql("ALTER TABLE account_spending_hourly DROP CONSTRAINT test_failure")
        .execute(&store.pool)
        .await
        .unwrap();
    let mut pending = Vec::new();
    for _ in 0..10 {
        let store = store.clone();
        let r = r.clone();
        pending.push(tokio::spawn(async move {
            store.finish_request(&r).await.unwrap();
        }));
    }
    for task in pending {
        task.await.unwrap();
    }
    // Concurrent distinct completions in the same bucket must not lose updates.
    let mut pending = Vec::new();
    for _ in 0..12 {
        let store = store.clone();
        let r = record(Some(id), now - Duration::days(2), Some("1"));
        pending.push(tokio::spawn(async move {
            finish(&store, &r).await;
        }));
    }
    for task in pending {
        task.await.unwrap();
    }
    // Replayed delivery/cost changes after finalization leave original values.
    r.valuation.as_mut().unwrap().cny = Some(Decimal::from(999));
    store.finish_request(&r).await.unwrap();
    finish(
        &store,
        &record(Some(id), now - Duration::days(32), Some("40")),
    )
    .await;
    sqlx::raw_sql("ALTER TABLE quota_snapshots DROP CONSTRAINT quota_snapshots_account_id_fkey")
        .execute(&store.pool)
        .await
        .unwrap();
    for (at, used) in [(now - Duration::days(3), 10.0), (now, 20.0)] {
        save_window(&store, id, 10080, at, now + Duration::days(4), used).await;
    }
    let before = store.spending_report(id, now, 900).await.unwrap();
    assert_eq!(before["last_7d"]["cny"], "22");
    assert_eq!(before["last_7d"]["requests"], 13);
    store
        .cleanup(&RuntimeSettings {
            request_retention_days: 1,
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(before, store.spending_report(id, now, 900).await.unwrap());
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM requests")
        .fetch_one(&store.pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM account_spending_entries")
        .fetch_one(&store.pool)
        .await
        .unwrap();
    assert_eq!(count, 13);
    let count: i64 =
        sqlx::query_scalar("SELECT sum(requests)::bigint FROM account_spending_hourly")
            .fetch_one(&store.pool)
            .await
            .unwrap();
    assert_eq!(count, 13);
    // Restart reconciliation follows the same transactional completion path.
    let mut interrupted = record(Some(id), now, None);
    interrupted.finished_at = None;
    store.begin_request(&interrupted).await.unwrap();
    assert_eq!(store.reconcile_interrupted().await.unwrap(), 1);
    assert_eq!(store.reconcile_interrupted().await.unwrap(), 0);
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM account_spending_entries WHERE request_id=$1")
            .bind(interrupted.id)
            .fetch_one(&store.pool)
            .await
            .unwrap();
    assert_eq!(count, 1);
    teardown(store, schema).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database; run python3 scripts/test.py"]
async fn account_period_amounts_and_weekly_estimate_use_matching_samples() {
    let (store, schema) = setup(true).await;
    let id = Uuid::new_v4();
    let other = Uuid::new_v4();
    let now = Utc::now().with_nanosecond(0).unwrap();
    let reset = now + Duration::days(5);
    // Isolated quota table avoids unrelated account credential fixtures.
    sqlx::raw_sql("ALTER TABLE quota_snapshots DROP CONSTRAINT quota_snapshots_account_id_fkey")
        .execute(&store.pool)
        .await
        .unwrap();
    for (account, minutes, cny, model) in [
        (id, 60, Some("10"), "m"),
        (id, 360, Some("20"), "m"),
        (id, 30, Some("7"), "gpt-5.3-codex-spark"),
        (id, 15, None, "m"),
        (id, 300, Some("3"), "m"),
        (id, 8 * 1440, Some("40"), "m"),
        (other, 60, Some("999"), "m"),
    ] {
        let mut r = record(Some(account), now - Duration::minutes(minutes), cny);
        r.upstream_model = Some(model.into());
        finish(&store, &r).await;
    }
    for (minutes, percent, resets_at) in [
        (120, 20.0, reset),
        (180, 100.0, reset - Duration::days(1)),
        (0, 30.0, reset),
    ] {
        let w = QuotaWindow {
            pool: "codex".into(),
            window_minutes: Some(10080),
            used_percent: percent,
            resets_at: Some(resets_at),
            observed_at: now - Duration::minutes(minutes),
            source: "test".into(),
        };
        sqlx::query("INSERT INTO quota_snapshots(account_id,pool,window_minutes,observed_at,data) VALUES($1,'codex',10080,$2,$3)")
            .bind(id).bind(w.observed_at).bind(json!(w)).execute(&store.pool).await.unwrap();
    }
    save_window(&store, id, 300, now, now + Duration::hours(2), 30.0).await;
    let report = store.spending_report(id, now, 900).await.unwrap();
    assert_eq!(report["last_5h"]["cny"], "17");
    assert_eq!(report["last_7d"]["cny"], "17");
    assert_eq!(report["last_5h"]["unpriced"], 1);
    let estimate = &report["weekly_estimate"];
    assert_eq!(estimate["sample_cny"], "10");
    assert_eq!(estimate["used_percent_delta"], 10.0);
    assert_eq!(
        estimate["total_cny"]
            .as_str()
            .unwrap()
            .parse::<Decimal>()
            .unwrap(),
        Decimal::from(100)
    );
    assert_eq!(estimate["unpriced"], 1);
    assert_eq!(
        store.spending_report(other, now, 900).await.unwrap()["weekly_estimate"]["total_cny"],
        Value::Null
    );
    assert_eq!(
        store
            .spending_report(id, now + Duration::hours(1), 900)
            .await
            .unwrap()["weekly_estimate"]["sample_stale"],
        true
    );
    teardown(store, schema).await;
}

async fn save_window(
    store: &PgStore,
    id: Uuid,
    minutes: i32,
    at: DateTime<Utc>,
    reset: DateTime<Utc>,
    used: f64,
) {
    let w = QuotaWindow {
        pool: "codex".into(),
        window_minutes: Some(i64::from(minutes)),
        used_percent: used,
        resets_at: Some(reset),
        observed_at: at,
        source: "test".into(),
    };
    sqlx::query("INSERT INTO quota_snapshots(account_id,pool,window_minutes,observed_at,data) VALUES($1,'codex',$2,$3,$4)")
        .bind(id).bind(minutes).bind(at).bind(json!(w)).execute(&store.pool).await.unwrap();
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database; run python3 scripts/test.py"]
async fn account_cycles_survive_dense_sampling_and_reset_independently() {
    let (store, schema) = setup(true).await;
    sqlx::raw_sql("ALTER TABLE quota_snapshots DROP CONSTRAINT quota_snapshots_account_id_fkey")
        .execute(&store.pool)
        .await
        .unwrap();
    let id = Uuid::new_v4();
    let now = Utc::now().with_nanosecond(0).unwrap();
    let reset = now + Duration::days(2);
    save_window(&store, id, 10080, now - Duration::days(4), reset, 10.0).await;
    finish(
        &store,
        &record(Some(id), now - Duration::days(3), Some("20")),
    )
    .await;
    finish(
        &store,
        &record(Some(id), now - Duration::hours(4), Some("7")),
    )
    .await;
    finish(
        &store,
        &record(Some(id), now - Duration::minutes(10), Some("3")),
    )
    .await;
    let w = QuotaWindow {
        pool: "codex".into(),
        window_minutes: Some(10080),
        used_percent: 30.0,
        resets_at: Some(reset),
        observed_at: now,
        source: "test".into(),
    };
    // Header observations can exceed 10,000 long before the current week ends.
    sqlx::query("INSERT INTO quota_snapshots(account_id,pool,window_minutes,observed_at,data) SELECT $1,'codex',10080,$2,$3 FROM generate_series(1,10001)")
        .bind(id).bind(now).bind(json!(w)).execute(&store.pool).await.unwrap();
    save_window(
        &store,
        id,
        300,
        now - Duration::hours(2),
        now + Duration::hours(1),
        90.0,
    )
    .await;
    save_window(
        &store,
        id,
        300,
        now - Duration::minutes(30),
        now + Duration::hours(1),
        0.0,
    )
    .await;
    save_window(&store, id, 300, now, now + Duration::hours(1), 10.0).await;
    let report = store.spending_report(id, now, 900).await.unwrap();
    assert_eq!(report["last_5h"]["cny"], "3");
    assert_eq!(report["last_5h"]["requests"], 1);
    assert_eq!(report["last_5h"]["input_tokens"], "1234");
    assert_eq!(report["last_5h"]["output_tokens"], "56");
    assert_eq!(report["last_7d"]["requests"], 3);
    assert_eq!(report["last_7d"]["input_tokens"], "3702");
    assert_eq!(report["last_7d"]["output_tokens"], "168");
    assert_eq!(report["last_7d"]["cny"], "30");
    assert_eq!(report["weekly_estimate"]["sample_cny"], "30");
    assert_eq!(report["weekly_estimate"]["used_percent_delta"], 20.0);
    let later = store
        .spending_report(id, now + Duration::minutes(20), 900)
        .await
        .unwrap();
    assert_eq!(report["last_7d"], later["last_7d"]);
    assert_eq!(
        report["weekly_estimate"]["sample_from"],
        later["weekly_estimate"]["sample_from"]
    );
    store
        .cleanup(&RuntimeSettings {
            request_retention_days: 1,
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(report, store.spending_report(id, now, 900).await.unwrap());
    // Missing/expired periods are unknown, not rolling spend disguised as zero.
    let expired = store.spending_report(id, reset, 900).await.unwrap();
    assert_eq!(expired["last_7d"]["cny"], Value::Null);
    assert_eq!(expired["weekly_estimate"]["total_cny"], Value::Null);
    let mut partial = record(Some(id), now - Duration::minutes(5), Some("1"));
    partial.usage.input_tokens = None;
    partial.usage.output_tokens = Some(7);
    finish(&store, &partial).await;
    store.finish_request(&partial).await.unwrap();
    let partial_report = store.spending_report(id, now, 900).await.unwrap();
    assert_eq!(partial_report["last_5h"]["requests"], 2);
    assert_eq!(partial_report["last_5h"]["input_tokens"], "1234");
    assert_eq!(partial_report["last_5h"]["output_tokens"], "63");
    assert_eq!(partial_report["last_5h"]["missing_tokens"], 1);
    teardown(store, schema).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database; run python3 scripts/test.py"]
async fn monthly_cycles_cover_thirty_days_and_survive_short_request_retention() {
    let (store, schema) = setup(true).await;
    sqlx::raw_sql("ALTER TABLE quota_snapshots DROP CONSTRAINT quota_snapshots_account_id_fkey")
        .execute(&store.pool)
        .await
        .unwrap();
    let id = Uuid::new_v4();
    let now = Utc::now().with_nanosecond(123_456_000).unwrap();
    let start = now - Duration::days(20);
    let reset = now + Duration::days(10);
    save_window(&store, id, 43200, start + Duration::hours(1), reset, 5.0).await;
    save_window(&store, id, 43200, now, reset, 30.0).await;
    for (at, cny, model) in [
        (start, Some("999"), "m"),
        (start + Duration::microseconds(1), Some("2"), "m"),
        (now - Duration::days(10), Some("3"), "m"),
        (now - Duration::hours(8), Some("5"), "m"),
        (now - Duration::hours(7), Some("4"), "gpt-5.3-codex-spark"),
        (now - Duration::hours(6), None, "m"),
    ] {
        let mut r = record(Some(id), at, cny);
        r.upstream_model = Some(model.into());
        finish(&store, &r).await;
    }
    finish(&store, &record(Some(Uuid::new_v4()), now, Some("999"))).await;
    let partial = store.spending_report(id, now, 900).await.unwrap();
    assert_eq!(partial["last_30d"]["cny"], "14");
    assert_eq!(partial["last_30d"]["requests"], 5);
    assert_eq!(partial["last_30d"]["input_tokens"], "6170");
    assert_eq!(partial["last_30d"]["output_tokens"], "280");
    assert_eq!(partial["last_30d"]["unpriced"], 1);
    assert_eq!(partial["last_30d"]["history_complete"], false);
    assert_eq!(
        partial["monthly_estimate"]["status"],
        "insufficient_history"
    );
    assert_eq!(partial["last_7d"]["cny"], Value::Null);
    // This fixture has complete old history; an upgrade cannot generally assume it.
    sqlx::query("UPDATE account_spending_coverage SET complete_since=$1")
        .bind(now - Duration::days(31))
        .execute(&store.pool)
        .await
        .unwrap();
    let complete = store.spending_report(id, now, 900).await.unwrap();
    assert_eq!(complete["last_30d"]["history_complete"], true);
    assert_eq!(complete["monthly_estimate"]["sample_cny"], "8");
    assert_eq!(complete["monthly_estimate"]["used_percent_delta"], 25.0);
    assert_eq!(complete["monthly_estimate"]["total_cny"], "32");
    store
        .cleanup(&RuntimeSettings {
            request_retention_days: 1,
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(complete, store.spending_report(id, now, 900).await.unwrap());
    let retained: i64 =
        sqlx::query_scalar("SELECT count(*) FROM quota_snapshots WHERE account_id=$1")
            .bind(id)
            .fetch_one(&store.pool)
            .await
            .unwrap();
    assert_eq!(retained, 2);
    // An independently reset monthly counter must stop including the old segment.
    save_window(&store, id, 43200, now + Duration::seconds(1), reset, 0.0).await;
    finish(
        &store,
        &record(Some(id), now + Duration::seconds(2), Some("1")),
    )
    .await;
    save_window(&store, id, 43200, now + Duration::seconds(3), reset, 2.0).await;
    let reset_report = store
        .spending_report(id, now + Duration::seconds(3), 900)
        .await
        .unwrap();
    assert_eq!(reset_report["last_30d"]["cny"], "1");
    assert_eq!(reset_report["last_30d"]["requests"], 1);
    assert_eq!(reset_report["monthly_estimate"]["sample_cny"], "1");
    assert_eq!(
        store.spending_report(id, reset, 900).await.unwrap()["last_30d"]["status"],
        "unknown_cycle"
    );
    teardown(store, schema).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database; run python3 scripts/test.py"]
async fn monthly_upgrade_backfills_retained_history_without_repricing_existing_ledger() {
    let (store, schema) = setup(false).await;
    let now = Utc::now();
    let id = Uuid::new_v4();
    let old = record(Some(id), now - Duration::days(20), Some("7"));
    let current = record(Some(id), now - Duration::days(2), Some("3"));
    for r in [&old, &current] {
        insert_historical(&store, r).await;
    }
    for sql in [
        MIGRATION,
        include_str!("../migrations/0011_account_cycle_tokens.sql"),
        include_str!("../migrations/0012_soft_delete_accounts_keys.sql"),
    ] {
        sqlx::raw_sql(sql).execute(&store.pool).await.unwrap();
    }
    // A persisted ledger valuation is authoritative even if legacy detail changed.
    sqlx::query("UPDATE requests SET data=jsonb_set(data,'{valuation,cny}','\"999\"') WHERE id=$1")
        .bind(current.id)
        .execute(&store.pool)
        .await
        .unwrap();
    sqlx::raw_sql(include_str!(
        "../migrations/0013_monthly_account_spending.sql"
    ))
    .execute(&store.pool)
    .await
    .unwrap();
    store.finish_request(&old).await.unwrap();
    let totals:(String,String,i64)=sqlx::query_as("SELECT sum(cny)::text,sum(input_tokens)::text,sum(requests)::bigint FROM account_spending_hourly WHERE account_id=$1")
        .bind(id).fetch_one(&store.pool).await.unwrap();
    assert_eq!(totals, ("10".into(), "2468".into(), 2));
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM account_spending_entries WHERE account_id=$1")
            .bind(id)
            .fetch_one(&store.pool)
            .await
            .unwrap();
    assert_eq!(count, 2);
    teardown(store, schema).await;
}
