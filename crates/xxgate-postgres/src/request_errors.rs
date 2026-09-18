use crate::PgStore;
use chrono::Utc;
use serde_json::Value;
use sqlx::QueryBuilder;
use xxgate_core::{Error, Result, protocol::RequestKind, reports::ErrorFilter};

impl PgStore {
    pub(crate) async fn error_report(&self, f: &ErrorFilter) -> Result<Value> {
        let (from, to) = f.window(Utc::now())?;
        let bucket_seconds = if to - from <= chrono::Duration::hours(1) {
            300i64
        } else if to - from <= chrono::Duration::hours(48) {
            3600
        } else {
            86400
        };
        // One statement keeps summary, groups, trend and pagination on the same
        // database snapshot. Read bounded scalar facts, never full request data.
        let mut q = QueryBuilder::new("WITH candidates AS (SELECT id,created_at,finished_at,state,model,account_id,key_id,
            client_session_id AS session_id,COALESCE(data->>'kind','responses') AS kind,
            COALESCE(data->>'error_code','unknown_error') AS code,
            left(COALESCE(data->>'error_message',''),600) AS message,
            COALESCE(data->'ingress_diagnostics'->>'failure_stage',
                CASE WHEN state='interrupted' THEN 'process'
                     WHEN data->'upstream_status' IS NOT NULL AND data->'upstream_status'<>'null'::jsonb THEN 'upstream_response'
                     WHEN (data->>'upstream_attempts')::int>0 THEN 'upstream_transport'
                     WHEN state='rejected' THEN 'ingress_validation' ELSE 'queue_or_prepare' END) AS stage,
            data->'upstream_status' AS upstream_status,
            data->'ingress_diagnostics'->'error'->'status' AS gateway_status,
            data->'upstream_error'->>'reason' AS cause,data->'upstream_error'->>'param' AS param,
            data->'upstream_error'->>'code' AS upstream_code,
            data->'body_bytes' AS body_bytes,data->'total_ms' AS total_ms,
            data->'upstream_attempts' AS upstream_attempts
            FROM requests WHERE created_at>=");
        q.push_bind(from).push(" AND created_at<").push_bind(to);
        if let Some(state) = &f.state {
            q.push(" AND state=").push_bind(state);
        } else {
            q.push(" AND state IN ('failed','rejected','interrupted')");
        }
        if let Some(id) = f.account_id {
            q.push(" AND account_id=").push_bind(id);
        }
        if let Some(id) = f.key_id {
            q.push(" AND key_id=").push_bind(id);
        }
        if let Some(model) = &f.model {
            q.push(" AND model=").push_bind(model);
        }
        if let Some(id) = f.request_id {
            q.push(" AND id=").push_bind(id);
        }
        if let Some(session) = &f.session_id {
            q.push(" AND client_session_id=").push_bind(session);
        }
        if let Some(kind) = f.kind {
            q.push(" AND COALESCE(data->>'kind','responses')=")
                .push_bind(match kind {
                    RequestKind::Responses => "responses",
                    RequestKind::Compact => "compact",
                    RequestKind::Search => "search",
                });
        }
        q.push("), filtered AS MATERIALIZED (SELECT * FROM candidates WHERE TRUE");
        for (field, value) in [
            ("code", &f.code),
            ("stage", &f.stage),
            ("cause", &f.cause),
            ("param", &f.param),
        ] {
            if let Some(value) = value {
                q.push(" AND COALESCE(")
                    .push(field)
                    .push(",'')=")
                    .push_bind(value);
            }
        }
        if let Some(status) = f.upstream_status {
            q.push(" AND upstream_status=")
                .push_bind(serde_json::json!(status));
        }
        if f.upstream_missing {
            q.push(" AND (upstream_status IS NULL OR upstream_status='null'::jsonb)");
        }
        if let Some(text) = &f.q {
            q.push(" AND position(lower(").push_bind(text)
                .push(") in lower(concat_ws(' ',code,message,cause,param,upstream_code,model,id::text,session_id)))>0");
        }
        q.push("), grouped AS (SELECT code,stage,upstream_status,cause,param,count(*) AS count,
            count(DISTINCT account_id) AS accounts,count(DISTINCT NULLIF(session_id,'')) AS sessions,
            min(created_at) AS first_seen,max(created_at) AS last_seen
            FROM filtered GROUP BY code,stage,upstream_status,cause,param),
            page AS (SELECT * FROM filtered ORDER BY created_at DESC,id DESC LIMIT ")
            .push_bind(f.limit.unwrap_or(25)).push(" OFFSET ").push_bind(f.offset.unwrap_or(0))
            .push("), points AS (SELECT date_bin(")
            .push_bind(match bucket_seconds { 300 => "5 minutes", 3600 => "1 hour", _ => "1 day" })
            .push("::interval,created_at,'2000-01-01'::timestamptz) AS at,count(*) AS count FROM filtered GROUP BY 1)
            SELECT jsonb_build_object('from',").push_bind(from).push(",'to',").push_bind(to)
            .push(",'limit',").push_bind(f.limit.unwrap_or(25)).push(",'offset',").push_bind(f.offset.unwrap_or(0))
            .push(",'bucket_seconds',").push_bind(bucket_seconds)
            .push(",'summary',(SELECT jsonb_build_object('total',count(*),'accounts',count(DISTINCT account_id),
                'sessions',count(DISTINCT NULLIF(session_id,'')),'last_seen',max(created_at),
                'groups',(SELECT count(*) FROM grouped)) FROM filtered),
            'groups',COALESCE((SELECT jsonb_agg(to_jsonb(g) ORDER BY count DESC,last_seen DESC,code,stage,upstream_status,cause,param) FROM
                (SELECT * FROM grouped ORDER BY count DESC,last_seen DESC,code,stage,upstream_status,cause,param LIMIT 20) g),'[]'::jsonb),
            'trend',COALESCE((SELECT jsonb_agg(to_jsonb(points) ORDER BY at) FROM points),'[]'::jsonb),
            'options',jsonb_build_object(
                'accounts',COALESCE((SELECT jsonb_agg(jsonb_build_object('id',id,'name',data->>'name') ORDER BY data->>'name',id) FROM accounts),'[]'::jsonb),
                'keys',COALESCE((SELECT jsonb_agg(jsonb_build_object('id',id,'name',name) ORDER BY name,id) FROM api_keys),'[]'::jsonb)),
            'items',COALESCE((SELECT jsonb_agg(to_jsonb(p)||jsonb_build_object(
                'account_name',a.data->>'name','key_name',k.name) ORDER BY p.created_at DESC,p.id DESC)
                FROM page p LEFT JOIN accounts a ON a.id=p.account_id LEFT JOIN api_keys k ON k.id=p.key_id),'[]'::jsonb))");
        q.build_query_scalar()
            .fetch_one(&self.pool)
            .await
            .map_err(|_error| {
                #[cfg(test)]
                eprintln!("error report SQL failed: {_error}");
                Error::storage()
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;
    use serde_json::json;
    use sqlx::Executor;
    use uuid::Uuid;

    #[tokio::test]
    #[ignore = "requires a disposable PostgreSQL database; run python3 scripts/test.py"]
    async fn summaries_filters_and_pages_share_the_same_error_population() {
        let db = std::env::var("TEST_DATABASE_URL").unwrap();
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .connect(&db)
            .await
            .unwrap();
        let schema = format!("error_report_{}", Uuid::new_v4().simple());
        sqlx::raw_sql(&format!(
            "CREATE SCHEMA {schema}; SET search_path TO {schema};"
        ))
        .execute(&pool)
        .await
        .unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        let store = PgStore { pool };
        let now = Utc::now();
        let account = Uuid::new_v4();
        let key = Uuid::new_v4();
        for i in 0..30u128 {
            let state = match i {
                0 => "completed",
                1 => "cancelled",
                2 => "inflight",
                3 => "rejected",
                4 => "interrupted",
                _ => "failed",
            };
            let at = if i == 5 {
                now - Duration::days(2)
            } else {
                now - Duration::minutes(i as i64 + 1)
            };
            let code = match i {
                3 => "memory_limit".into(),
                4 => "process_interrupted".into(),
                6..=8 => "upstream_invalid_request".into(),
                _ => format!("error_{i}"),
            };
            let mut data = json!({"kind":"responses","error_code":code,"error_message":"Safe explanation 100%_literal", "upstream_attempts":1,
                "upstream_status":400,"body_bytes":123,"total_ms":300,
                "ingress_diagnostics":{"private":"PRIVATE_DIAGNOSTIC"},"usage":{"raw_usage":"PRIVATE_USAGE".repeat(5000)}});
            if i == 3 {
                data["upstream_status"] = Value::Null;
                data["upstream_attempts"] = json!(0);
                data["ingress_diagnostics"]["failure_stage"] = json!("body_read");
                data["ingress_diagnostics"]["error"] = json!({"status":429});
            }
            if i == 4 {
                data.as_object_mut().unwrap().remove("upstream_status");
                data["upstream_attempts"] = json!(0);
            }
            if i == 6 || i == 7 {
                data["upstream_error"] = json!({"reason":"unsupported_parameter","param":"max_output_tokens","code":"unknown_parameter"});
            }
            if i == 8 {
                data["upstream_status"] = json!(200);
            }
            sqlx::query("INSERT INTO requests(id,key_id,account_id,client_session_id,model,state,created_at,finished_at,data) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9)")
                .bind(Uuid::from_u128(i+1)).bind(key).bind(if i==3 {None}else{Some(account)}).bind(if i==3 {""}else{"session"})
                .bind("test-model").bind(state).bind(at).bind(at+Duration::milliseconds(300)).bind(data).execute(&store.pool).await.unwrap();
        }
        let f = ErrorFilter {
            from: Some(now - Duration::hours(24)),
            to: Some(now),
            limit: Some(2),
            ..Default::default()
        };
        let d = store.error_report(&f).await.unwrap();
        assert_eq!(d["summary"]["total"], 26);
        assert_eq!(d["summary"]["accounts"], 1);
        assert_eq!(d["summary"]["sessions"], 1);
        assert_eq!(d["items"].as_array().unwrap().len(), 2);
        assert_eq!(d["groups"].as_array().unwrap().len(), 20);
        assert_eq!(d["summary"]["groups"], 25);
        assert_eq!(
            d["trend"]
                .as_array()
                .unwrap()
                .iter()
                .map(|p| p["count"].as_i64().unwrap())
                .sum::<i64>(),
            26
        );
        assert!(!d.to_string().contains("PRIVATE_"));
        assert!(d.to_string().len() < 30_000);
        let next = store
            .error_report(&ErrorFilter {
                offset: Some(2),
                ..f.clone()
            })
            .await
            .unwrap();
        assert_ne!(d["items"][0]["id"], next["items"][0]["id"]);
        assert_eq!(next["summary"], d["summary"]);
        let grouped = store
            .error_report(&ErrorFilter {
                code: Some("upstream_invalid_request".into()),
                cause: Some("unsupported_parameter".into()),
                param: Some("max_output_tokens".into()),
                upstream_status: Some(400),
                ..f.clone()
            })
            .await
            .unwrap();
        assert_eq!(grouped["summary"]["total"], 2);
        let local = store
            .error_report(&ErrorFilter {
                stage: Some("body_read".into()),
                upstream_missing: true,
                ..f.clone()
            })
            .await
            .unwrap();
        assert_eq!(local["summary"]["total"], 1);
        assert_eq!(local["items"][0]["gateway_status"], 429);
        assert!(local["items"][0]["upstream_status"].is_null());
        assert_eq!(
            store
                .error_report(&ErrorFilter {
                    upstream_status: Some(200),
                    ..f.clone()
                })
                .await
                .unwrap()["summary"]["total"],
            1
        );
        assert_eq!(
            store
                .error_report(&ErrorFilter {
                    account_id: Some(account),
                    ..f.clone()
                })
                .await
                .unwrap()["summary"]["total"],
            25
        );
        assert_eq!(
            store
                .error_report(&ErrorFilter {
                    state: Some("cancelled".into()),
                    ..f.clone()
                })
                .await
                .unwrap()["summary"]["total"],
            1
        );
        assert_eq!(
            store
                .error_report(&ErrorFilter {
                    q: Some("MAX_OUTPUT_TOKENS".into()),
                    ..f.clone()
                })
                .await
                .unwrap()["summary"]["total"],
            2
        );
        assert_eq!(
            store
                .error_report(&ErrorFilter {
                    q: Some("100%_literal".into()),
                    ..f.clone()
                })
                .await
                .unwrap()["summary"]["total"],
            26
        );
        let empty = store
            .error_report(&ErrorFilter {
                q: Some("%' OR TRUE --".into()),
                ..f.clone()
            })
            .await
            .unwrap();
        assert_eq!(empty["summary"]["total"], 0);
        assert_eq!(empty["groups"], json!([]));
        assert_eq!(empty["trend"], json!([]));
        assert_eq!(
            store
                .error_report(&ErrorFilter {
                    request_id: Some(Uuid::from_u128(7)),
                    ..f.clone()
                })
                .await
                .unwrap()["items"][0]["param"],
            "max_output_tokens"
        );
        for bad in [
            ErrorFilter {
                state: Some("completed".into()),
                ..f.clone()
            },
            ErrorFilter {
                from: Some(now - Duration::days(32)),
                ..f.clone()
            },
            ErrorFilter {
                upstream_status: Some(99),
                ..f.clone()
            },
            ErrorFilter {
                offset: Some(-1),
                ..f.clone()
            },
            ErrorFilter {
                q: Some("x".repeat(513)),
                ..f.clone()
            },
        ] {
            assert!(store.error_report(&bad).await.is_err());
        }
        store
            .pool
            .execute(format!("DROP SCHEMA {schema} CASCADE").as_str())
            .await
            .unwrap();
    }
}
