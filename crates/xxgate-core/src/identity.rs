use crate::{Error, Result, types::ModelRef};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SessionKey {
    pub key_id: Uuid,
    pub client_session_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ThreadKey {
    pub session: SessionKey,
    pub client_thread_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientIdentity {
    pub session_id: String,
    pub thread_id: String,
    pub turn_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Binding {
    pub id: Uuid,
    #[serde(default = "crate::groups::default_group_id")]
    pub group_id: Uuid,
    pub session: SessionKey,
    pub generation: i64,
    pub account_id: Uuid,
    pub namespace: Uuid,
    pub provider: String,
    pub access_kind: String,
}

impl Binding {
    pub fn new(session: SessionKey, account_id: Uuid, model: &ModelRef, generation: i64) -> Self {
        Self {
            id: Uuid::new_v4(),
            group_id: crate::groups::DEFAULT_GROUP_ID,
            session,
            generation,
            account_id,
            namespace: Uuid::new_v4(),
            provider: model.provider.clone(),
            access_kind: model.access_kind.clone(),
        }
    }
    pub fn compatible(&self, model: &ModelRef) -> bool {
        self.provider == model.provider && self.access_kind == model.access_kind
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct IdMapping {
    pub kind: String,
    pub client_id: String,
    pub upstream_id: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RequestRewrite {
    pub entries: Vec<IdentifierRewrite>,
    pub omitted: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IdentifierRewrite {
    pub field: String,
    pub before: Option<String>,
    pub after: Option<String>,
    pub action: String,
}

pub struct IdentityMap {
    pub binding: Binding,
    forward: HashMap<(String, String), String>,
    reverse: HashMap<(String, String), String>,
    legacy_aliases: HashMap<(String, String), String>,
    pending: Vec<IdMapping>,
    request_rewrite: Option<RequestRewrite>,
}

impl IdentityMap {
    pub fn new(binding: Binding, mappings: Vec<IdMapping>) -> Self {
        let mut this = Self {
            binding,
            forward: HashMap::new(),
            reverse: HashMap::new(),
            legacy_aliases: HashMap::new(),
            pending: Vec::new(),
            request_rewrite: None,
        };
        this.import_legacy_aliases(&this.binding.clone(), &mappings);
        for m in mappings {
            this.forward
                .insert((m.kind.clone(), m.client_id.clone()), m.upstream_id.clone());
            this.reverse.insert((m.kind, m.upstream_id), m.client_id);
        }
        this
    }
    pub fn import_legacy_aliases(&mut self, binding: &Binding, mappings: &[IdMapping]) {
        if binding.session != self.binding.session
            || binding.provider != self.binding.provider
            || binding.access_kind != self.binding.access_kind
        {
            return;
        }
        for mapping in mappings {
            if !matches!(mapping.kind.as_str(), "item" | "call" | "response") {
                continue;
            }
            // Old outbound mappings used UUID v5 of the client identifier.
            // Those are transformed input IDs and must now be ignored.
            // Other entries are aliases issued for upstream output IDs: resolve
            // them so clients of an older gateway can replay their history.
            let generated = Uuid::new_v5(
                &binding.namespace,
                format!("{}\0{}", mapping.kind, mapping.client_id).as_bytes(),
            )
            .simple()
            .to_string();
            if mapping.upstream_id.rsplit('_').next() != Some(generated.as_str()) {
                self.legacy_aliases
                    .entry((mapping.kind.clone(), mapping.client_id.clone()))
                    .or_insert_with(|| mapping.upstream_id.clone());
            }
        }
    }
    pub fn original_id<'a>(&'a self, kind: &str, id: &'a str) -> &'a str {
        if id.len() > 512 {
            return id;
        }
        self.legacy_aliases
            .get(&(kind.into(), id.into()))
            .map_or(id, String::as_str)
    }
    pub fn existing(&self, kind: &str, client: &str) -> Option<&str> {
        self.forward
            .get(&(kind.into(), client.into()))
            .map(String::as_str)
    }
    pub fn outbound(&mut self, kind: &str, client: &str) -> Result<String> {
        if client.is_empty() || client.len() > 512 {
            return Err(Error::invalid("Identity length is invalid"));
        }
        if let Some(v) = self.existing(kind, client) {
            return Ok(v.to_owned());
        }
        let id = Uuid::new_v5(
            &self.binding.namespace,
            format!("{kind}\0{client}").as_bytes(),
        )
        .simple()
        .to_string();
        let upstream = match kind {
            "item" => format!("{}_{}", item_prefix(client), id),
            "call" => format!("call_{id}"),
            "response" => format!("resp_{id}"),
            _ => Uuid::parse_str(&id)
                .map_err(|_| Error::invalid("Invalid identity"))?
                .to_string(),
        };
        self.insert(kind, client.to_owned(), upstream.clone());
        Ok(upstream)
    }
    pub fn inbound(&mut self, kind: &str, upstream: &str) -> Result<String> {
        if upstream.is_empty() || upstream.len() > 512 {
            return Err(Error::new(
                502,
                "invalid_upstream_id",
                "Upstream identity length is invalid",
            ));
        }
        if let Some(client) = self.reverse.get(&(kind.into(), upstream.into())) {
            return Ok(client.clone());
        }
        let prefix = match kind {
            "response" => "resp",
            "call" => "call",
            _ => item_prefix(upstream),
        };
        let client = format!("{prefix}_{}", Uuid::new_v4().simple());
        self.insert(kind, client.clone(), upstream.into());
        Ok(client)
    }
    fn insert(&mut self, kind: &str, client_id: String, upstream_id: String) {
        self.forward
            .insert((kind.into(), client_id.clone()), upstream_id.clone());
        self.reverse
            .insert((kind.into(), upstream_id.clone()), client_id.clone());
        self.pending.push(IdMapping {
            kind: kind.into(),
            client_id,
            upstream_id,
        });
    }
    pub fn take_pending(&mut self) -> Vec<IdMapping> {
        std::mem::take(&mut self.pending)
    }
    pub fn record_request_rewrite(&mut self, rewrite: RequestRewrite) {
        self.request_rewrite = Some(rewrite);
    }
    pub fn take_request_rewrite(&mut self) -> Option<RequestRewrite> {
        self.request_rewrite.take()
    }
}

fn item_prefix(id: &str) -> &str {
    // The prefix denotes the protocol item type (including tool outputs such
    // as ctco/fco). Preserve it instead of mapping unfamiliar types to msg.
    id.split_once('_')
        .map(|(prefix, _)| prefix)
        .filter(|prefix| {
            !prefix.is_empty()
                && prefix.len() <= 64
                && prefix.bytes().all(|byte| byte.is_ascii_alphanumeric())
        })
        .unwrap_or("msg")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn item_type_prefixes_survive_mapping_in_both_directions() {
        let binding = Binding::new(
            SessionKey {
                key_id: Uuid::new_v4(),
                client_session_id: "s".into(),
            },
            Uuid::new_v4(),
            &ModelRef::codex("m"),
            1,
        );
        let mut ids = IdentityMap::new(binding, vec![]);
        for prefix in ["msg", "rs", "ctc", "ctco", "fc", "fco", "future123"] {
            let client = format!("{prefix}_client");
            let outbound = ids.outbound("item", &client).unwrap();
            assert!(outbound.starts_with(&format!("{prefix}_")));
            assert_ne!(outbound, client);
            assert_eq!(ids.inbound("item", &outbound).unwrap(), client);
            let upstream = format!("{prefix}_upstream");
            let inbound = ids.inbound("item", &upstream).unwrap();
            assert!(inbound.starts_with(&format!("{prefix}_")));
            assert_eq!(ids.outbound("item", &inbound).unwrap(), upstream);
        }
        let mut restored = IdentityMap::new(ids.binding.clone(), ids.take_pending());
        assert!(
            restored
                .outbound("item", "ctco_client")
                .unwrap()
                .starts_with("ctco_")
        );
        assert!(restored.take_pending().is_empty());
    }
    #[test]
    fn migration_and_return_never_reuse_ids() {
        let session = SessionKey {
            key_id: Uuid::new_v4(),
            client_session_id: "session".into(),
        };
        let account = Uuid::new_v4();
        let model = ModelRef::codex("test");
        let mut a = IdentityMap::new(Binding::new(session.clone(), account, &model, 1), vec![]);
        let mut b = IdentityMap::new(
            Binding::new(session.clone(), Uuid::new_v4(), &model, 2),
            vec![],
        );
        let mut c = IdentityMap::new(Binding::new(session, account, &model, 3), vec![]);
        let id = a.outbound("thread", "session").unwrap();
        assert_eq!(id, a.outbound("thread", "session").unwrap());
        assert_ne!(id, b.outbound("thread", "session").unwrap());
        assert_ne!(id, c.outbound("thread", "session").unwrap());
        let restored = IdentityMap::new(a.binding.clone(), a.take_pending());
        assert_eq!(restored.existing("thread", "session"), Some(id.as_str()));
    }
}
