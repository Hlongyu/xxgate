use super::{AppState, admin::account_identity};
use serde_json::Value;
use uuid::Uuid;
use xxgate_core::{Result, accounts::Account, clients::ClientSource, providers::ModelSpec};

fn plan_rank(plan: Option<&str>) -> u8 {
    match plan {
        Some("pro") => 0,
        Some("plus") => 1,
        Some("free") => 2,
        _ => 3,
    }
}

pub(super) async fn catalog(
    state: &AppState,
    group_id: Uuid,
    source: ClientSource,
    models: &[ModelSpec],
) -> Result<Vec<Value>> {
    let mut accounts = Vec::new();
    for account in state.gateway.scheduler.accounts() {
        if eligible(&account, group_id, source)
            && models
                .iter()
                .any(|model| capability(&account, model).is_some())
        {
            // Read existing claims only: listing models never refreshes credentials.
            let identity = account_identity(state, account.id).await?;
            let rank = plan_rank(identity.as_ref().and_then(|i| i.plan_type.as_deref()));
            accounts.push((rank, account));
        }
    }
    Ok(select(accounts, group_id, source, models))
}

fn eligible(account: &Account, group_id: Uuid, source: ClientSource) -> bool {
    account.enabled && account.group_ids.contains(&group_id) && account.accepts(source)
}

fn capability<'a>(account: &'a Account, model: &ModelSpec) -> Option<&'a Value> {
    if !model.enabled || !account.supports(&model.upstream) {
        return None;
    }
    account
        .model_catalog
        .as_ref()?
        .models
        .iter()
        .find(|m| m.id == model.upstream.model)?
        .raw
        .as_ref()
        .filter(|raw| raw.is_object())
}

fn select(
    mut accounts: Vec<(u8, Account)>,
    group_id: Uuid,
    source: ClientSource,
    models: &[ModelSpec],
) -> Vec<Value> {
    accounts.sort_by_key(|(rank, account)| (*rank, account.id));
    models
        .iter()
        .filter_map(|model| {
            let raw = accounts
                .iter()
                .filter(|(_, account)| eligible(account, group_id, source))
                .find_map(|(_, account)| capability(account, model))?;
            let mut raw = raw.clone();
            // The advertised slug must be usable as the gateway request model.
            raw["slug"] = Value::String(model.id.clone());
            Some(raw)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use xxgate_core::{
        accounts::ClientProfile,
        providers::{AccountModelCatalog, DiscoveredModel},
        types::ModelRef,
    };

    fn account(id: u128, raw: Option<Value>) -> Account {
        let mut a: Account = serde_json::from_value(json!({
            "id":Uuid::from_u128(id),"name":"test","provider":"openai","access_kind":"codex_oauth",
            "enabled":true,"max_inflight":1,"upstream_account_id":"test","upstream_base_url":"https://example.com",
            "models":[],"profile":ClientProfile::default(),"version":1,"credential_version":1,
            "created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-01T00:00:00Z"
        })).unwrap();
        a.model_catalog = Some(AccountModelCatalog {
            models: vec![DiscoveredModel {
                id: "upstream".into(),
                display_name: "Test".into(),
                context_window: None,
                raw,
            }],
            synced_at: Some(chrono::Utc::now()),
            ..Default::default()
        });
        a
    }

    fn model() -> ModelSpec {
        ModelSpec {
            id: "alias".into(),
            upstream: ModelRef::codex("upstream"),
            enabled: true,
            capabilities: Default::default(),
            version: 1,
        }
    }

    #[test]
    fn picks_whole_object_by_plan_then_stable_account_id() {
        let pro = account(
            3,
            Some(json!({"slug":"upstream","future":{"a":[1,null]},"service_tiers":[]})),
        );
        let group = pro.group_ids[0];
        let mut candidates = vec![
            (
                plan_rank(Some("free")),
                account(1, Some(json!({"slug":"upstream","free_only":true}))),
            ),
            (
                plan_rank(Some("plus")),
                account(2, Some(json!({"slug":"upstream","plus_only":true}))),
            ),
            (
                plan_rank(Some("pro")),
                account(4, Some(json!({"slug":"upstream","other_pro":true}))),
            ),
            (plan_rank(Some("pro")), pro),
            (
                plan_rank(None),
                account(0, Some(json!({"slug":"upstream","unknown":true}))),
            ),
        ];
        let selected = select(candidates.clone(), group, ClientSource::Unknown, &[model()]);
        assert_eq!(
            selected,
            vec![json!({"slug":"alias","future":{"a":[1,null]},"service_tiers":[]})]
        );
        candidates.reverse();
        assert_eq!(
            select(candidates.clone(), group, ClientSource::Unknown, &[model()]),
            selected
        );
        candidates.retain(|(rank, _)| *rank != 0);
        assert_eq!(
            select(candidates.clone(), group, ClientSource::Unknown, &[model()])[0]["plus_only"],
            true
        );
        candidates.retain(|(rank, _)| *rank != 1);
        assert_eq!(
            select(candidates, group, ClientSource::Unknown, &[model()])[0]["free_only"],
            true
        );
    }

    #[test]
    fn excludes_ineligible_accounts_and_legacy_catalogs() {
        let base = account(1, Some(json!({"slug":"upstream","chosen":true})));
        let group = base.group_ids[0];
        for condition in 0..6 {
            let mut a = base.clone();
            match condition {
                0 => a.enabled = false,
                1 => a.group_ids.clear(),
                2 => a.codex_only = true,
                3 => a.models_restricted = true,
                4 => a.model_catalog.as_mut().unwrap().models[0].raw = None,
                _ => a.provider = "other".into(),
            }
            assert!(select(vec![(0, a)], group, ClientSource::Unknown, &[model()]).is_empty());
        }
        let mut disabled = model();
        disabled.enabled = false;
        assert!(select(vec![(0, base)], group, ClientSource::Unknown, &[disabled]).is_empty());
        let legacy: DiscoveredModel =
            serde_json::from_value(json!({"id":"old","display_name":"Old","context_window":null}))
                .unwrap();
        assert!(legacy.raw.is_none());
    }
}
