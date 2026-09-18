use bytes::Bytes;
use http::{Method, header::CONTENT_TYPE};
use serde_json::json;
use xxgate_core::{
    Error, Result,
    accounts::{Account, Credentials},
    protocol::PreparedRequest,
    resets::{ResetCredits, ResetOperation, ResetResult},
};

pub(super) fn request(
    a: &Account,
    c: &Credentials,
    operation: Option<&ResetOperation>,
) -> Result<PreparedRequest> {
    if a.provider != "openai" || a.access_kind != "codex_oauth" {
        return Err(Error::invalid("此账户不支持 Codex 额度重置"));
    }
    let mut request = super::quota::request(a, c)?;
    request.url = request.url.trim_end_matches("/usage").to_owned() + "/rate-limit-reset-credits";
    if let Some(operation) = operation {
        request.method = Method::POST;
        request.url.push_str("/consume");
        request
            .headers
            .insert(CONTENT_TYPE, "application/json".parse().unwrap());
        request.body = Bytes::from(
            json!({"redeem_request_id":operation.id,"credit_id":operation.credit_id}).to_string(),
        );
    }
    Ok(request)
}

pub(super) fn credits(body: &[u8]) -> Result<ResetCredits> {
    let mut credits: ResetCredits = serde_json::from_slice(body).map_err(|_| invalid_response())?;
    let mut ids = std::collections::HashSet::new();
    if credits
        .credits
        .iter()
        .any(|c| c.id.trim().is_empty() || c.id.len() > 512 || !ids.insert(&c.id))
    {
        return Err(invalid_response());
    }
    credits.observed_at = chrono::Utc::now();
    credits.sort();
    Ok(credits)
}

pub(super) fn result(body: &[u8]) -> Result<ResetResult> {
    serde_json::from_slice(body).map_err(|_| invalid_response())
}

fn invalid_response() -> Error {
    Error::new(
        502,
        "invalid_reset_response",
        "上游重置响应无法识别，请重新查询或核实原操作",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, Utc};
    use xxgate_core::resets::ResetCode;

    #[test]
    fn unknown_or_malformed_credits_cannot_be_spent() {
        let now = Utc::now();
        let entry = |id: &str, status: &str, kind: &str, expiration| {
            json!({
                "id":id,"status":status,"reset_type":kind,"granted_at":now,"expires_at":expiration
            })
        };
        let payload = json!({"available_count":6,"credits":[
            entry("infinite","available","codex_rate_limits",None),
            entry("later","available","codex_rate_limits",Some(now+Duration::days(2))),
            entry("soon","available","codex_rate_limits",Some(now+Duration::days(1))),
            entry("expired","available","codex_rate_limits",Some(now)),
            entry("new-status","new","codex_rate_limits",Some(now)),
            entry("new-type","available","new",Some(now)),
        ]});
        let parsed = credits(payload.to_string().as_bytes()).unwrap();
        assert_eq!(parsed.credits.len(), 6);
        assert_eq!(parsed.next(now).unwrap().id, "soon");
        assert_eq!(parsed.next(now + Duration::days(1)).unwrap().id, "later");
        assert_eq!(parsed.next(now + Duration::days(3)).unwrap().id, "infinite");
        for invalid in [
            json!({"available_count":0}),
            json!({"credits":[],"available_count":-1}),
            json!({"available_count":1,"credits":[entry("bad","available","codex_rate_limits",None)],"observed_at":"bad-date"}),
            json!({"available_count":2,"credits":[payload["credits"][0].clone(),payload["credits"][0].clone()]}),
        ] {
            assert!(credits(invalid.to_string().as_bytes()).is_err());
        }
        assert_eq!(
            credits(br#"{"available_count":0,"credits":[]}"#)
                .unwrap()
                .credits
                .len(),
            0
        );
    }

    #[test]
    fn only_known_redemption_outcomes_are_final() {
        for (code, expected) in [
            ("reset", ResetCode::Reset),
            ("no_credit", ResetCode::NoCredit),
            ("nothing_to_reset", ResetCode::NothingToReset),
            ("already_redeemed", ResetCode::AlreadyRedeemed),
        ] {
            assert_eq!(
                result(json!({"code":code}).to_string().as_bytes())
                    .unwrap()
                    .code,
                expected
            );
        }
        assert!(result(br#"{"code":"new_outcome"}"#).is_err());
        assert!(result(br#"{}"#).is_err());
    }
}
