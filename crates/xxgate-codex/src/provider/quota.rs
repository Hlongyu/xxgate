use bytes::Bytes;
use chrono::Utc;
use http::{HeaderMap, Method};
use serde_json::Value;
use xxgate_core::{
    Error, Result,
    accounts::{Account, Credentials},
    protocol::PreparedRequest,
    quota::QuotaWindow,
};

pub(crate) fn headers(h: &HeaderMap) -> Vec<QuotaWindow> {
    let mut result = vec![];
    for name in h.keys() {
        let name = name.as_str();
        if let Some(base) = name.strip_suffix("-used-percent") {
            if !base.starts_with("x-")
                || !(base.ends_with("-primary") || base.ends_with("-secondary"))
            {
                continue;
            }
            let Some(percent) = h
                .get(name)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<f64>().ok())
                .filter(|v| v.is_finite() && *v >= 0.0)
            else {
                continue;
            };
            let number = |suffix: &str| {
                h.get(format!("{base}-{suffix}"))
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.parse::<i64>().ok())
            };
            let pool = base
                .trim_start_matches("x-")
                .rsplit_once('-')
                .map(|(p, _)| p)
                .unwrap_or("codex");
            result.push(QuotaWindow {
                pool: pool.replace('-', "_"),
                window_minutes: number("window-minutes"),
                used_percent: percent,
                resets_at: number("reset-at").and_then(|n| chrono::DateTime::from_timestamp(n, 0)),
                observed_at: Utc::now(),
                source: "response_header".into(),
            });
        }
    }
    result
}

pub(crate) fn request(a: &Account, c: &Credentials) -> Result<PreparedRequest> {
    let base = a.upstream_base_url.trim_end_matches('/');
    let base = base.strip_suffix("/codex").unwrap_or(base);
    Ok(PreparedRequest {
        method: Method::GET,
        url: format!("{base}/wham/usage"),
        headers: super::request::auth_headers(a, c)?,
        body: Bytes::new(),
        account_id: Some(a.id),
        profile_version: a.version,
        tls_backend: a.profile.tls_backend.clone(),
    })
}

pub(crate) fn response(body: &[u8]) -> Result<Vec<QuotaWindow>> {
    let value: Value = serde_json::from_slice(body).map_err(|_| {
        Error::new(
            502,
            "invalid_quota_response",
            "Upstream quota response is invalid",
        )
    })?;
    Ok(windows(&value, "usage_query"))
}

pub(crate) fn windows(value: &Value, source: &str) -> Vec<QuotaWindow> {
    let mut result = vec![];
    if let Some(array) = value.as_array() {
        for entry in array {
            result.extend(windows(entry, source));
        }
        return result;
    }
    let pool = value
        .get("metered_limit_name")
        .or_else(|| value.get("limit_id"))
        .or_else(|| value.get("limit_name"))
        .and_then(Value::as_str)
        .unwrap_or("codex");
    for root in [
        value.get("rate_limit"),
        value.get("rate_limits"),
        Some(value),
    ]
    .into_iter()
    .flatten()
    {
        for key in ["primary_window", "secondary_window", "primary", "secondary"] {
            let Some(w) = root.get(key) else {
                continue;
            };
            let Some(percent) = w
                .get("used_percent")
                .and_then(Value::as_f64)
                .filter(|p| p.is_finite() && *p >= 0.0)
            else {
                continue;
            };
            let minutes = w.get("window_minutes").and_then(Value::as_i64).or_else(|| {
                w.get("limit_window_seconds")
                    .and_then(Value::as_i64)
                    .map(|v| v / 60)
            });
            let reset = w
                .get("reset_at")
                .or_else(|| w.get("resets_at"))
                .and_then(Value::as_i64)
                .and_then(|n| chrono::DateTime::from_timestamp(n, 0));
            if !result
                .iter()
                .any(|e: &QuotaWindow| e.pool == pool && e.window_minutes == minutes)
            {
                result.push(QuotaWindow {
                    pool: pool.replace('-', "_"),
                    window_minutes: minutes,
                    used_percent: percent,
                    resets_at: reset,
                    observed_at: Utc::now(),
                    source: source.into(),
                });
            }
        }
    }
    for key in ["additional_rate_limits", "rate_limits_by_limit_id"] {
        if let Some(array) = value.get(key).and_then(Value::as_array) {
            for entry in array {
                result.extend(windows(entry, source));
            }
        }
        if let Some(map) = value.get(key).and_then(Value::as_object) {
            for (pool, item) in map {
                let mut item = item.clone();
                item["limit_id"] = Value::String(pool.clone());
                result.extend(windows(&item, source));
            }
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn free_thirty_day_window_matches_usage_and_headers_and_can_disable() {
        let body = serde_json::json!({"plan_type":"free","rate_limit":{"primary_window":{"used_percent":100,"limit_window_seconds":2592000,"reset_at":1800000000},"secondary_window":null}});
        let from_usage = response(&serde_json::to_vec(&body).unwrap()).unwrap();
        let mut h = HeaderMap::new();
        for (key, value) in [
            ("x-codex-primary-used-percent", "100"),
            ("x-codex-primary-window-minutes", "43200"),
            ("x-codex-primary-reset-at", "1800000000"),
        ] {
            h.insert(key, value.parse().unwrap());
        }
        let from_headers = headers(&h);
        for windows in [&from_usage, &from_headers] {
            assert_eq!(windows.len(), 1);
            assert_eq!(windows[0].window_minutes, Some(43200));
            assert_eq!(windows[0].used_percent, 100.0);
            assert_eq!(
                windows[0].disable_reason(),
                Some(xxgate_core::accounts::DisableReason::QuotaExhausted)
            );
        }
        assert_eq!(from_usage[0].resets_at, from_headers[0].resets_at);
    }
}
