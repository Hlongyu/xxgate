use super::accounts::{PgStore, decode, encode, event_tx};
use async_trait::async_trait;
use chrono::Utc;
use rust_decimal::Decimal;
use serde_json::{Value, json};
use sqlx::{Postgres, QueryBuilder, Row};
use uuid::Uuid;
use xxgate_core::{
    Error, Result,
    application::ports::{ReportStore, RequestStore},
    audit::{AuditEvent, RequestRecord},
    reports::{RequestFilter, UsageFilter},
    settings::RuntimeSettings,
};

#[async_trait]
impl RequestStore for PgStore {
    async fn safety_rejection(
        &self,
        session: &xxgate_core::identity::SessionKey,
    ) -> Result<Option<Error>> {
        sqlx::query("SELECT data FROM safety_rejections WHERE key_id=$1 AND session_id=$2")
            .bind(session.key_id)
            .bind(&session.client_session_id)
            .fetch_optional(&self.pool)
            .await
            .map_err(|_| Error::storage())?
            .as_ref()
            .map(decode)
            .transpose()
    }
    async fn save_safety_rejection(
        &self,
        session: &xxgate_core::identity::SessionKey,
        error: &Error,
    ) -> Result<()> {
        sqlx::query("INSERT INTO safety_rejections(key_id,session_id,data) VALUES($1,$2,$3) ON CONFLICT(key_id,session_id) DO NOTHING")
            .bind(session.key_id).bind(&session.client_session_id).bind(encode(error)?)
            .execute(&self.pool).await.map_err(|_| Error::storage())?;
        Ok(())
    }
    async fn begin_request(&self, r: &RequestRecord) -> Result<()> {
        let mut tx = self.pool.begin().await.map_err(|_| Error::storage())?;
        sqlx::query("INSERT INTO requests(id,key_id,account_id,client_session_id,model,state,created_at,data) VALUES($1,$2,$3,$4,$5,$6,$7,$8)")
            .bind(r.id).bind(r.key_id).bind(r.account_id).bind(&r.client_session_id).bind(&r.model).bind(&r.state).bind(r.created_at).bind(encode(r)?).execute(&mut *tx).await.map_err(|_|Error::storage())?;
        event_tx(&mut tx,&AuditEvent::new(if r.state=="rejected" {"request_rejected"}else{"request_accepted"},"gateway",Some(r.id),None,json!({"group_id":r.group_id,"kind":r.kind,"client_origin":r.client_origin,"stateless":r.stateless,"stream":r.stream,"model":r.model,"body_bytes":r.body_bytes,"config_version":r.config_version,"error_code":r.error_code,"diagnostics":if r.state=="rejected" {r.ingress_diagnostics.as_ref()}else{None}}))).await?;
        tx.commit().await.map_err(|_| Error::storage())?;
        Ok(())
    }
    async fn update_request(&self, r: &RequestRecord) -> Result<()> {
        sqlx::query("UPDATE requests SET account_id=$2,state=$3,data=$4 WHERE id=$1 AND finished_at IS NULL").bind(r.id).bind(r.account_id).bind(&r.state).bind(encode(r)?).execute(&self.pool).await.map_err(|_|Error::storage())?;
        Ok(())
    }
    async fn finish_request(&self, r: &RequestRecord) -> Result<()> {
        let mut tx = self.pool.begin().await.map_err(|_| Error::storage())?;
        let finished_at = r.finished_at.unwrap_or_else(Utc::now);
        let rows=sqlx::query("UPDATE requests SET account_id=$2,state=$3,finished_at=$4,data=$5 WHERE id=$1 AND finished_at IS NULL")
            .bind(r.id).bind(r.account_id).bind(&r.state).bind(finished_at).bind(encode(r)?).execute(&mut *tx).await.map_err(|_|Error::storage())?.rows_affected();
        if rows == 0 {
            return Ok(());
        }
        let cost = r.valuation.as_ref().and_then(|v| v.cny);
        sqlx::query("INSERT INTO request_hourly(hour,account_id,key_id,provider,model,service_tier,requests,completed,failed,cancelled,input_tokens,output_tokens,cached_tokens,image_count,cny,unpriced,incomplete_usage,duration_ms,search_calls,search_cny)
            VALUES(date_trunc('hour',$1::timestamptz),$2,$3,$4,$5,$6,1,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,$18,$19)
            ON CONFLICT(hour,account_id,key_id,provider,model,service_tier) DO UPDATE SET
            requests=request_hourly.requests+1,completed=request_hourly.completed+EXCLUDED.completed,failed=request_hourly.failed+EXCLUDED.failed,cancelled=request_hourly.cancelled+EXCLUDED.cancelled,
            input_tokens=request_hourly.input_tokens+EXCLUDED.input_tokens,output_tokens=request_hourly.output_tokens+EXCLUDED.output_tokens,cached_tokens=request_hourly.cached_tokens+EXCLUDED.cached_tokens,
            image_count=request_hourly.image_count+EXCLUDED.image_count,cny=request_hourly.cny+EXCLUDED.cny,unpriced=request_hourly.unpriced+EXCLUDED.unpriced,incomplete_usage=request_hourly.incomplete_usage+EXCLUDED.incomplete_usage,duration_ms=request_hourly.duration_ms+EXCLUDED.duration_ms,search_calls=request_hourly.search_calls+EXCLUDED.search_calls,search_cny=request_hourly.search_cny+EXCLUDED.search_cny")
            .bind(r.created_at).bind(r.account_id.unwrap_or_else(Uuid::nil)).bind(r.key_id).bind(&r.provider).bind(&r.model).bind(r.usage.service_tier.as_deref().unwrap_or("unknown"))
            .bind(i64::from(r.state=="completed")).bind(i64::from(r.state=="failed" || r.state=="interrupted" || r.state=="rejected")).bind(i64::from(r.state=="cancelled"))
            .bind(Decimal::from(r.usage.input_tokens.unwrap_or(0))).bind(Decimal::from(r.usage.output_tokens.unwrap_or(0))).bind(Decimal::from(r.usage.cached_input_tokens.unwrap_or(0)))
            .bind(i64::from(r.usage.image_count)).bind(cost.unwrap_or(Decimal::ZERO)).bind(i64::from(cost.is_none())).bind(i64::from(!r.usage.complete && r.upstream_attempts>0))
            .bind(Decimal::from(r.total_ms.unwrap_or(0))).bind(i64::try_from(r.usage.search_calls).map_err(|_|Error::storage())?).bind(if r.kind == xxgate_core::protocol::RequestKind::Search { cost.unwrap_or(Decimal::ZERO) } else { Decimal::ZERO }).execute(&mut *tx).await.map_err(|_|Error::storage())?;
        if let Some(account_id) = r.account_id {
            sqlx::query(include_str!("account_spending_increment.sql"))
                .bind(r.id)
                .bind(account_id)
                .bind(finished_at)
                .bind(cost)
                .bind(
                    r.upstream_attempts > 0
                        && r.upstream_model.as_deref().unwrap_or(&r.model) != "gpt-5.3-codex-spark",
                )
                .bind(r.usage.input_tokens.map(Decimal::from))
                .bind(r.usage.output_tokens.map(Decimal::from))
                .execute(&mut *tx)
                .await
                .map_err(|_| Error::storage())?;
        }
        event_tx(&mut tx,&AuditEvent::new("request_finished","gateway",Some(r.id),r.account_id,json!({"group_id":r.group_id,"state":r.state,"error_code":r.error_code,"total_ms":r.total_ms,"usage_complete":r.usage.complete,"valuation_status":r.valuation.as_ref().map(|v|&v.status),"config_versions":r.config_versions}))).await?;
        tx.commit().await.map_err(|_| Error::storage())?;
        Ok(())
    }
    async fn append_event(&self, event: &AuditEvent) -> Result<()> {
        let mut tx = self.pool.begin().await.map_err(|_| Error::storage())?;
        event_tx(&mut tx, event).await?;
        tx.commit().await.map_err(|_| Error::storage())?;
        Ok(())
    }
    async fn reconcile_interrupted(&self) -> Result<u64> {
        let rows = sqlx::query("SELECT data FROM requests WHERE finished_at IS NULL")
            .fetch_all(&self.pool)
            .await
            .map_err(|_| Error::storage())?;
        for row in &rows {
            let mut r: RequestRecord = decode(row)?;
            r.finished_at = Some(Utc::now());
            r.state = "interrupted".into();
            r.error_code = Some("process_interrupted".into());
            r.error_message = Some(
                "Process exited before a final outcome was persisted; request was not replayed"
                    .into(),
            );
            r.total_ms = Some((Utc::now() - r.created_at).num_milliseconds().max(0) as u64);
            r.usage.complete = false;
            if r.kind == xxgate_core::protocol::RequestKind::Search {
                r.valuation = Some(xxgate_core::pricing::value_search(
                    r.usage.search_calls,
                    r.search_price.as_ref(),
                ));
            }
            self.finish_request(&r).await?;
        }
        Ok(rows.len() as u64)
    }
}

fn request_where<'a>(q: &mut QueryBuilder<'a, Postgres>, f: &'a RequestFilter) {
    q.push(" WHERE TRUE");
    if let Some(source) = f.client_source {
        q.push(" AND data->'client_origin'->>'source'=")
            .push_bind(match source {
                xxgate_core::clients::ClientSource::Codex => "codex",
                xxgate_core::clients::ClientSource::Unknown => "unknown",
            });
    }
    if let Some(kind) = f.kind {
        q.push(" AND COALESCE(data->>'kind','responses')=")
            .push_bind(match kind {
                xxgate_core::protocol::RequestKind::Search => "search",
                xxgate_core::protocol::RequestKind::Responses => "responses",
                xxgate_core::protocol::RequestKind::Compact => "compact",
            });
    }
    if let Some(v) = f.account_id {
        q.push(" AND account_id=").push_bind(v);
    }
    if let Some(v) = f.key_id {
        q.push(" AND key_id=").push_bind(v);
    }
    if let Some(v) = &f.state {
        q.push(" AND state=").push_bind(v);
    }
    if let Some(v) = &f.model {
        q.push(" AND model=").push_bind(v);
    }
    if let Some(v) = &f.session_id {
        q.push(" AND client_session_id=").push_bind(v);
    }
    if let Some(v) = f.from {
        q.push(" AND created_at>=").push_bind(v);
    }
    if let Some(v) = f.to {
        q.push(" AND created_at<").push_bind(v);
    }
    if let Some(v) = f.id {
        q.push(" AND id=").push_bind(v);
    }
}

// Whitelist fields rendered in the list (including cache comparisons and billing
// popovers). New diagnostics or upstream usage fields must never expand this API.
fn list_usage(data: &str) -> String {
    format!(
        "jsonb_build_object('input_tokens',{data}->'input_tokens',
        'output_tokens',{data}->'output_tokens','cached_input_tokens',{data}->'cached_input_tokens',
        'complete',{data}->'complete','service_tier',{data}->'service_tier',
        'search_calls',{data}->'search_calls')"
    )
}

// The predecessor is independent of the visible filters and pagination. Search
// and rejected requests cannot warm a model cache, so they are not baselines.
fn request_report_query(summary: bool) -> String {
    let usage = list_usage("requests.data->'usage'");
    let record = if summary {
        format!("jsonb_build_object(
            'id',requests.id,'key_id',requests.key_id,'account_id',requests.account_id,
            'client_session_id',requests.client_session_id,'model',requests.model,
            'state',requests.state,'created_at',requests.created_at,
            'kind',requests.data->'kind','stateless',requests.data->'stateless',
            'stream',requests.data->'stream','client_origin',requests.data->'client_origin',
            'provider',requests.data->'provider','upstream_model',requests.data->'upstream_model',
            'binding_id',requests.data->'binding_id','binding_generation',requests.data->'binding_generation',
            'reasoning_effort',requests.data->'reasoning_effort','requested_tier',requests.data->'requested_tier',
            'queue_ms',requests.data->'queue_ms','first_content_ms',requests.data->'first_content_ms',
            'total_ms',requests.data->'total_ms','usage',{usage},
            'valuation',requests.data->'valuation','search_price',requests.data->'search_price')")
    } else {
        "requests.data".into()
    };
    let previous_usage = if summary {
        list_usage("p.data->'usage'")
    } else {
        "p.data->'usage'".into()
    };
    let price = if summary {
        "jsonb_build_object('standard',data->'standard','fast_multiplier',data->'fast_multiplier')"
    } else {
        "data"
    };
    format!("SELECT {record} || jsonb_build_object(
        'account_name', (SELECT data->>'name' FROM accounts WHERE accounts.id=requests.account_id),
        'price', (SELECT {price} FROM prices WHERE version=(requests.data->'valuation'->>'price_version')::bigint),
        'cache_previous', (SELECT jsonb_build_object(
            'id',p.id,'created_at',p.created_at,'finished_at',p.finished_at,
            'account_id',p.account_id,'binding_id',p.data->'binding_id',
            'binding_generation',p.data->'binding_generation',
            'provider',p.data->'provider','model',p.model,'upstream_model',p.data->'upstream_model',
            'usage',{previous_usage}) FROM requests p
          WHERE COALESCE(requests.data->>'kind','responses')='responses'
            AND requests.client_session_id<>''
            AND p.key_id=requests.key_id AND p.client_session_id=requests.client_session_id
            AND p.data->>'client_thread_id'=requests.data->>'client_thread_id'
            AND COALESCE(p.data->>'kind','responses')='responses'
            AND (p.data->>'upstream_attempts')::int>0
            AND (p.created_at,p.id)<(requests.created_at,requests.id)
          ORDER BY p.created_at DESC,p.id DESC LIMIT 1)
        ) AS data FROM {} requests", if summary { "page" } else { "requests" })
}

#[async_trait]
impl ReportStore for PgStore {
    async fn request_errors(&self, filter: &xxgate_core::reports::ErrorFilter) -> Result<Value> {
        self.error_report(filter).await
    }
    async fn account_spending(
        &self,
        id: Uuid,
        now: chrono::DateTime<Utc>,
        stale_seconds: u64,
    ) -> Result<Value> {
        self.spending_report(id, now, stale_seconds).await
    }
    async fn requests(&self, filter: &RequestFilter) -> Result<Value> {
        let mut count = QueryBuilder::new("SELECT count(*) FROM requests");
        request_where(&mut count, filter);
        let total: i64 = count
            .build_query_scalar()
            .fetch_one(&self.pool)
            .await
            .map_err(|_| Error::storage())?;
        // Look up the immutable price version captured by this request, never
        // today's model price. Account names reflect the current display name.
        // Enrich only the selected page, never every row matching the filter.
        let mut q = QueryBuilder::new("WITH page AS MATERIALIZED (SELECT * FROM requests");
        request_where(&mut q, filter);
        q.push(" ORDER BY created_at DESC,id DESC LIMIT ")
            .push_bind(filter.limit.unwrap_or(50).clamp(1, 500))
            .push(" OFFSET ")
            .push_bind(filter.offset.unwrap_or(0).max(0))
            .push(") ")
            .push(request_report_query(true))
            .push(" ORDER BY requests.created_at DESC,requests.id DESC");
        let rows = q
            .build()
            .fetch_all(&self.pool)
            .await
            .map_err(|_| Error::storage())?;
        let data = rows
            .iter()
            .map(|r| r.try_get::<Value, _>("data").map_err(|_| Error::storage()))
            .collect::<Result<Vec<_>>>()?;
        Ok(json!({"items":data,"total":total}))
    }
    async fn request_detail(&self, id: Uuid) -> Result<Value> {
        let row = sqlx::query(&format!("{} WHERE id=$1", request_report_query(false)))
            .bind(id)
            .fetch_optional(&self.pool)
            .await
            .map_err(|_| Error::storage())?
            .ok_or_else(Error::not_found)?;
        let r: RequestRecord = decode(&row)?;
        let events =
            sqlx::query("SELECT data FROM audit_events WHERE request_id=$1 ORDER BY at,id")
                .bind(id)
                .fetch_all(&self.pool)
                .await
                .map_err(|_| Error::storage())?
                .iter()
                .map(|r| r.try_get::<Value, _>("data").map_err(|_| Error::storage()))
                .collect::<Result<Vec<_>>>()?;
        let bindings=sqlx::query("SELECT data FROM bindings WHERE key_id=$1 AND client_session_id=$2 ORDER BY generation").bind(r.key_id).bind(&r.client_session_id).fetch_all(&self.pool).await.map_err(|_|Error::storage())?.iter().map(|r|r.try_get::<Value,_>("data").map_err(|_|Error::storage())).collect::<Result<Vec<_>>>()?;
        let mappings = if let Some(id) = r.binding_id {
            sqlx::query("SELECT kind,client_id,upstream_id FROM identity_mappings WHERE binding_id=$1 ORDER BY kind,client_id LIMIT 500").bind(id).fetch_all(&self.pool).await.map_err(|_|Error::storage())?.iter().map(|r| Ok(json!({"kind":r.try_get::<String,_>("kind").map_err(|_|Error::storage())?,"client_id":r.try_get::<String,_>("client_id").map_err(|_|Error::storage())?,"upstream_id":r.try_get::<String,_>("upstream_id").map_err(|_|Error::storage())?}))).collect::<Result<Vec<Value>>>()?
        } else {
            vec![]
        };
        let enrichment: Value = row.try_get("data").map_err(|_| Error::storage())?;
        let mut report = encode(&r)?;
        for field in ["account_name", "price", "cache_previous"] {
            report[field] = enrichment[field].clone();
        }
        Ok(json!({"request":report,"events":events,"bindings":bindings,"mappings":mappings}))
    }
    async fn dashboard(&self, filter: &UsageFilter) -> Result<Value> {
        filter.validate()?;
        let summary: Value=sqlx::query_scalar("SELECT jsonb_build_object('requests',COALESCE(sum(requests),0),'completed',COALESCE(sum(completed),0),'failed',COALESCE(sum(failed),0),'cancelled',COALESCE(sum(cancelled),0),'input_tokens',COALESCE(sum(input_tokens),0)::text,'output_tokens',COALESCE(sum(output_tokens),0)::text,'cached_tokens',COALESCE(sum(cached_tokens),0)::text,'images',COALESCE(sum(image_count),0),'search_calls',COALESCE(sum(search_calls),0),'search_cny',COALESCE(sum(search_cny),0)::text,'cny',COALESCE(sum(cny),0)::text,'unpriced',COALESCE(sum(unpriced),0),'incomplete_usage',COALESCE(sum(incomplete_usage),0),'average_ms',COALESCE(sum(duration_ms)/NULLIF(sum(requests),0),0)) FROM request_hourly WHERE ($1::uuid IS NULL OR account_id=$1) AND ($2::uuid IS NULL OR key_id=$2) AND ($3::text IS NULL OR model=$3) AND ($4::text IS NULL OR service_tier=$4) AND ($5::timestamptz IS NULL OR hour >= $5) AND ($6::timestamptz IS NULL OR hour < $6)").bind(filter.account_id).bind(filter.key_id).bind(&filter.model).bind(&filter.service_tier).bind(filter.from).bind(filter.to).fetch_one(&self.pool).await.map_err(|_|Error::storage())?;
        let hourly=sqlx::query("SELECT hour,COALESCE(sum(requests),0)::bigint AS requests,COALESCE(sum(failed),0)::bigint AS failed,COALESCE(sum(input_tokens+output_tokens),0)::text AS tokens FROM request_hourly WHERE hour>=now()-interval '24 hours' AND ($1::uuid IS NULL OR account_id=$1) AND ($2::uuid IS NULL OR key_id=$2) AND ($3::text IS NULL OR model=$3) AND ($4::text IS NULL OR service_tier=$4) AND ($5::timestamptz IS NULL OR hour >= $5) AND ($6::timestamptz IS NULL OR hour < $6) GROUP BY hour ORDER BY hour").bind(filter.account_id).bind(filter.key_id).bind(&filter.model).bind(&filter.service_tier).bind(filter.from).bind(filter.to).fetch_all(&self.pool).await.map_err(|_|Error::storage())?.iter().map(|r|Ok(json!({"hour":r.try_get::<chrono::DateTime<Utc>,_>("hour").map_err(|_|Error::storage())?,"requests":r.try_get::<i64,_>("requests").map_err(|_|Error::storage())?,"failed":r.try_get::<i64,_>("failed").map_err(|_|Error::storage())?,"tokens":r.try_get::<String,_>("tokens").map_err(|_|Error::storage())?}))).collect::<Result<Vec<Value>>>()?;
        let models=sqlx::query("SELECT model,COALESCE(sum(requests),0)::bigint AS requests,COALESCE(sum(cny),0)::text AS cny,COALESCE(sum(search_calls),0)::bigint AS search_calls,COALESCE(sum(search_cny),0)::text AS search_cny FROM request_hourly WHERE ($1::uuid IS NULL OR account_id=$1) AND ($2::uuid IS NULL OR key_id=$2) AND ($3::text IS NULL OR model=$3) AND ($4::text IS NULL OR service_tier=$4) AND ($5::timestamptz IS NULL OR hour >= $5) AND ($6::timestamptz IS NULL OR hour < $6) GROUP BY model ORDER BY requests DESC").bind(filter.account_id).bind(filter.key_id).bind(&filter.model).bind(&filter.service_tier).bind(filter.from).bind(filter.to).fetch_all(&self.pool).await.map_err(|_|Error::storage())?.iter().map(|r|Ok(json!({"model":r.try_get::<String,_>("model").map_err(|_|Error::storage())?,"requests":r.try_get::<i64,_>("requests").map_err(|_|Error::storage())?,"cny":r.try_get::<String,_>("cny").map_err(|_|Error::storage())?,"search_calls":r.try_get::<i64,_>("search_calls").map_err(|_|Error::storage())?,"search_cny":r.try_get::<String,_>("search_cny").map_err(|_|Error::storage())?}))).collect::<Result<Vec<Value>>>()?;
        Ok(json!({"summary":summary,"hourly":hourly,"models":models}))
    }
    async fn audit_events(&self, offset: i64, limit: i64) -> Result<Value> {
        let rows=sqlx::query("SELECT data FROM audit_events WHERE request_id IS NULL ORDER BY at DESC LIMIT $1 OFFSET $2").bind(limit.clamp(1,200)).bind(offset.max(0)).fetch_all(&self.pool).await.map_err(|_|Error::storage())?;
        let items = rows
            .iter()
            .map(|r| r.try_get::<Value, _>("data").map_err(|_| Error::storage()))
            .collect::<Result<Vec<_>>>()?;
        let total: i64 =
            sqlx::query_scalar("SELECT count(*) FROM audit_events WHERE request_id IS NULL")
                .fetch_one(&self.pool)
                .await
                .map_err(|_| Error::storage())?;
        Ok(json!({"items":items,"total":total}))
    }
    async fn cleanup(&self, cfg: &RuntimeSettings) -> Result<Value> {
        let mut tx = self.pool.begin().await.map_err(|_| Error::storage())?;
        // Preserve current quota-cycle statistics even when request details have
        // a shorter retention. Delete only complete hours from both tables.
        sqlx::query("DELETE FROM account_spending_entries WHERE finished_at < date_trunc('hour',now()-interval '31 days','UTC')")
            .execute(&mut *tx).await.map_err(|_|Error::storage())?;
        sqlx::query("DELETE FROM account_spending_hourly WHERE hour < date_trunc('hour',now()-interval '31 days','UTC')")
            .execute(&mut *tx).await.map_err(|_|Error::storage())?;
        let requests=sqlx::query("DELETE FROM requests WHERE finished_at IS NOT NULL AND created_at<now()-make_interval(days=>$1)").bind(cfg.request_retention_days as i32).execute(&mut *tx).await.map_err(|_|Error::storage())?.rows_affected();
        let events=sqlx::query("DELETE FROM audit_events WHERE (request_id IS NOT NULL AND at<now()-make_interval(days=>$1)) OR (request_id IS NULL AND at<now()-make_interval(days=>$2))").bind(cfg.request_retention_days as i32).bind(cfg.audit_retention_days as i32).execute(&mut *tx).await.map_err(|_|Error::storage())?.rows_affected();
        sqlx::query("DELETE FROM admin_sessions WHERE expires_at<now()")
            .execute(&mut *tx)
            .await
            .map_err(|_| Error::storage())?;
        sqlx::query("DELETE FROM quota_snapshots WHERE observed_at<now()-make_interval(days=>$1) AND id NOT IN (SELECT max(id) FROM quota_snapshots GROUP BY account_id,pool,window_minutes)").bind(cfg.request_retention_days.max(31) as i32).execute(&mut *tx).await.map_err(|_|Error::storage())?;
        tx.commit().await.map_err(|_| Error::storage())?;
        Ok(
            json!({"config_version":cfg.version,"requests_deleted":requests,"events_deleted":events,"finished_at":Utc::now()}),
        )
    }
    async fn health(&self) -> Result<()> {
        sqlx::query("SELECT 1")
            .execute(&self.pool)
            .await
            .map_err(|_| Error::storage())?;
        Ok(())
    }
}

#[cfg(test)]
mod cache_tests {
    use super::*;

    #[tokio::test]
    #[ignore = "requires a disposable PostgreSQL database; run python3 scripts/test.py"]
    async fn cache_predecessor_is_isolated_and_independent_of_filters_and_pages() {
        let url = std::env::var("TEST_DATABASE_URL").unwrap();
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .connect(&url)
            .await
            .unwrap();
        let schema = format!("cache_test_{}", Uuid::new_v4().simple());
        sqlx::raw_sql(&format!(
            "CREATE SCHEMA {schema}; SET search_path TO {schema};"
        ))
        .execute(&pool)
        .await
        .unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        let store = PgStore { pool };
        let key = Uuid::new_v4();
        let start = Utc::now();
        let base = RequestRecord {
            client_turn_state: None,
            client_origin: None,
            stateless: false,
            id: Uuid::from_u128(1),
            key_id: key,
            client_session_id: "shared-session".into(),
            client_thread_id: "thread-a".into(),
            kind: xxgate_core::protocol::RequestKind::Responses,
            compaction: None,
            model: "old-model".into(),
            provider: "openai".into(),
            state: "failed".into(),
            usage: xxgate_core::usage::Usage {
                input_tokens: Some(1000),
                cached_input_tokens: Some(900),
                complete: true,
                raw_usage: json!({"large_upstream_extension": "x".repeat(32_768)}),
                ..Default::default()
            },
            created_at: start,
            finished_at: Some(start + chrono::Duration::milliseconds(100)),
            account_id: Some(Uuid::new_v4()),
            binding_id: None,
            binding_generation: Some(1),
            ingress_diagnostics: Some(json!({"test_diagnostic": "detail only"})),
            stream: Some(true),
            search_price: None,
            group_id: None,
            upstream_model: None,
            response_model: None,
            requested_tier: None,
            reasoning_effort: None,
            queue_ms: None,
            first_event_ms: None,
            first_content_ms: None,
            total_ms: Some(100),
            upstream_status: Some(200),
            upstream_headers_ms: None,
            upstream_request_id: None,
            error_code: None,
            error_message: None,
            upstream_error: None,
            valuation: None,
            config_version: 1,
            config_versions: vec![1],
            body_bytes: 0,
            upstream_attempts: 1,
        };
        let mut records = vec![base.clone(); 8];
        for (i, r) in records.iter_mut().enumerate() {
            r.id = Uuid::from_u128(i as u128 + 1);
            r.created_at = start + chrono::Duration::seconds(i as i64);
            r.finished_at = Some(r.created_at + chrono::Duration::milliseconds(100));
        }
        records[1].key_id = Uuid::new_v4();
        records[2].client_thread_id = "thread-b".into();
        records[3].client_session_id = "other-session".into();
        records[4].kind = xxgate_core::protocol::RequestKind::Search;
        records[5].state = "rejected".into();
        records[5].upstream_attempts = 0;
        records[6].model = "new-model".into();
        records[6].response_model = Some("returned-model".into());
        records[6].state = "completed".into();
        records[7].created_at = records[6].created_at; // UUID provides a stable tie break.
        for r in &records {
            sqlx::query("INSERT INTO requests(id,key_id,account_id,client_session_id,model,state,created_at,finished_at,data) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9)")
                .bind(r.id).bind(r.key_id).bind(r.account_id).bind(&r.client_session_id).bind(&r.model)
                .bind(&r.state).bind(r.created_at).bind(r.finished_at).bind(encode(r).unwrap())
                .execute(&store.pool).await.unwrap();
        }
        let filtered = store
            .requests(&RequestFilter {
                id: Some(records[6].id),
                key_id: Some(key),
                model: Some("new-model".into()),
                state: Some("completed".into()),
                from: Some(records[6].created_at - chrono::Duration::milliseconds(1)),
                limit: Some(1),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(filtered["total"], 1);
        let expected = base.id.to_string();
        assert_eq!(filtered["items"][0]["cache_previous"]["id"], expected);
        assert_eq!(filtered["items"][0]["cache_previous"]["model"], "old-model");
        let page = store
            .requests(&RequestFilter {
                key_id: Some(key),
                limit: Some(1),
                offset: Some(1),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(page["items"][0]["id"], records[6].id.to_string());
        assert_eq!(page["items"][0]["cache_previous"]["id"], expected);
        let detail = store.request_detail(records[6].id).await.unwrap();
        let summary = &filtered["items"][0];
        assert!(summary.get("response_model").is_none());
        assert_eq!(detail["request"]["response_model"], "returned-model");
        assert!(
            store.request_detail(records[0].id).await.unwrap()["request"]["response_model"]
                .is_null()
        );
        assert!(summary.get("ingress_diagnostics").is_none());
        assert!(summary.get("config_versions").is_none());
        assert!(summary["usage"].get("raw_usage").is_none());
        assert!(
            summary["cache_previous"]["usage"]
                .get("raw_usage")
                .is_none()
        );
        assert_eq!(
            detail["request"]["ingress_diagnostics"],
            records[6].ingress_diagnostics.clone().unwrap()
        );
        assert_eq!(
            detail["request"]["usage"]["raw_usage"],
            base.usage.raw_usage
        );
        assert_eq!(
            detail["request"]["cache_previous"]["usage"]["raw_usage"],
            base.usage.raw_usage
        );
        for (field, value) in summary["cache_previous"].as_object().unwrap() {
            if field == "usage" {
                for (key, value) in value.as_object().unwrap() {
                    assert_eq!(&detail["request"]["cache_previous"]["usage"][key], value);
                }
            } else {
                assert_eq!(&detail["request"]["cache_previous"][field], value);
            }
        }
        assert!(summary.to_string().len() * 10 < detail["request"].to_string().len());
        assert_eq!(
            store.request_detail(records[7].id).await.unwrap()["request"]["cache_previous"]["id"],
            records[6].id.to_string()
        );
        for i in [0, 1, 2, 3, 4] {
            assert!(
                store.request_detail(records[i].id).await.unwrap()["request"]["cache_previous"]
                    .is_null()
            );
        }
        sqlx::raw_sql(&format!("DROP SCHEMA {schema} CASCADE"))
            .execute(&store.pool)
            .await
            .unwrap();
        store.pool.close().await;
    }
}
