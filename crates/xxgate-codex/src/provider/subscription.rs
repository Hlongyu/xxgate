//! Read-only subscription metadata. Never select a different workspace as a fallback.
use chrono::{DateTime, Utc};
use serde_json::Value;
use xxgate_core::{Error, Result, accounts::SubscriptionSnapshot};

fn plan(value: &Value) -> Option<String> {
    let p = value.as_str()?.trim().to_ascii_lowercase();
    matches!(
        p.as_str(),
        "free" | "plus" | "pro" | "go" | "team" | "business" | "enterprise" | "edu"
    )
    .then_some(p)
}
fn date(value: &Value) -> Result<Option<DateTime<Utc>>> {
    if value.is_null() || value.as_str() == Some("") {
        return Ok(None);
    }
    value
        .as_str()
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|d| Some(d.with_timezone(&Utc)))
        .ok_or_else(invalid)
}
fn invalid() -> Error {
    Error::new(
        502,
        "subscription_invalid_response",
        "上游未提供可识别的套餐信息；已保留上次成功记录",
    )
}

pub fn accounts_check(value: &Value, account_id: &str) -> Result<SubscriptionSnapshot> {
    let accounts = value["accounts"].as_object().ok_or_else(invalid)?;
    let (_, entry) = accounts
        .iter()
        .find(|(key, v)| {
            v["account"]["account_id"]
                .as_str()
                .filter(|s| !s.is_empty())
                .unwrap_or(key)
                == account_id
        })
        .ok_or_else(|| {
            Error::new(
                502,
                "subscription_account_missing",
                "上游套餐列表没有匹配的账户；已保留上次成功记录",
            )
        })?;
    let plan_type = plan(&entry["account"]["plan_type"])
        .or_else(|| plan(&entry["entitlement"]["subscription_plan"]))
        .ok_or_else(invalid)?;
    Ok(SubscriptionSnapshot {
        upstream_account_id: account_id.into(),
        active_until: if plan_type == "free" {
            None
        } else {
            date(&entry["entitlement"]["expires_at"])?
        },
        plan_type,
        will_renew: None,
        last_checked_at: Utc::now(),
        source: "accounts_check".into(),
    })
}

pub fn subscriptions(value: &Value, account_id: &str) -> Result<SubscriptionSnapshot> {
    // `id` is the subscription ID, not the account ID. Only validate the explicit
    // account_id if supplied; the request itself is scoped with account_id.
    if value
        .get("account_id")
        .is_some_and(|id| id.as_str() != Some(account_id))
    {
        return Err(invalid());
    }
    let plan_type = plan(&value["plan_type"]).ok_or_else(invalid)?;
    let will_renew = match value.get("will_renew") {
        None | Some(Value::Null) => None,
        Some(Value::Bool(v)) => Some(*v),
        _ => return Err(invalid()),
    };
    Ok(SubscriptionSnapshot {
        upstream_account_id: account_id.into(),
        active_until: if plan_type == "free" {
            None
        } else {
            date(&value["active_until"])?
        },
        plan_type,
        will_renew,
        last_checked_at: Utc::now(),
        source: "subscriptions".into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn does_not_borrow_workspace_dates() {
        let v = json!({"accounts":{"default":{"account":{"account_id":"team","plan_type":"team"},"entitlement":{"expires_at":"2031-01-01T00:00:00Z"}},"personal":{"account":{"plan_type":"pro"},"entitlement":{"expires_at":null}}}});
        assert!(accounts_check(&v, "missing").is_err());
        let snapshot = accounts_check(&v, "personal").unwrap();
        assert_eq!(snapshot.plan_type, "pro");
        assert!(snapshot.active_until.is_none());
        assert!(accounts_check(&v, "default").is_err());
    }
    #[test]
    fn parses_dates_without_inferring_token_expiry() {
        let v = json!({"plan_type":"plus","active_until":"2030-01-01T08:00:00+08:00","will_renew":false,"id":"subscription-id"});
        let s = subscriptions(&v, "personal").unwrap();
        assert_eq!(
            s.active_until.unwrap().to_rfc3339(),
            "2030-01-01T00:00:00+00:00"
        );
        assert_eq!(s.will_renew, Some(false));
        assert!(
            subscriptions(&json!({"plan_type":"pro","exp":2000000000}), "personal")
                .unwrap()
                .active_until
                .is_none()
        );
        for bad in [
            json!({}),
            json!({"plan_type":"pro","active_until":123}),
            json!({"plan_type":"pro","active_until":"bad"}),
            json!({"plan_type":"pro","account_id":"other"}),
        ] {
            assert!(subscriptions(&bad, "personal").is_err());
        }
    }
}
