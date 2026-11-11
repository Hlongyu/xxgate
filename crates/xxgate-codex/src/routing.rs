//! A router forwards original bytes. It needs identities and routing options,
//! not owned conversation strings, tool schemas or image data. RawValue still
//! validates the entire JSON envelope; the worker performs full validation.
use serde_json::{Value, value::RawValue};
use std::collections::BTreeMap;
use xxgate_core::memory::MemoryAdmission;
use xxgate_core::memory_json::{self, ParseError};

pub fn projection(raw: &[u8], gate: &MemoryAdmission) -> Result<Value, ParseError> {
    gate.check(true).map_err(ParseError::Memory)?;
    let fields: BTreeMap<String, &RawValue> =
        serde_json::from_slice(raw).map_err(|_| ParseError::Invalid)?;
    let mut out = serde_json::Map::new();
    for (name, value) in fields {
        let projected = match name.as_str() {
            "input" => match value.get().as_bytes().first() {
                Some(b'[') => Value::Array(Vec::new()),
                Some(b'"') => Value::String(String::new()),
                _ => Value::Null,
            },
            "model"
            | "client_metadata"
            | "prompt_cache_key"
            | "reasoning"
            | "service_tier"
            | "stream"
            | "background"
            | "previous_response_id"
            | "generate"
            | "type" => memory_json::parse(value.get().as_bytes(), gate)?,
            _ => Value::Null,
        };
        out.insert(name, projected);
    }
    gate.check(true).map_err(ParseError::Memory)?;
    Ok(Value::Object(out))
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::HeaderMap;
    use serde_json::json;
    use std::sync::Arc;
    use xxgate_core::{
        memory::{MemoryDomain, MemoryProbe, MemorySample},
        protocol::IngressAdapter,
        settings::LiveSettings,
    };
    struct Healthy;
    impl MemoryProbe for Healthy {
        fn sample(&self) -> Result<MemorySample, &'static str> {
            Ok(MemorySample {
                domains: vec![MemoryDomain {
                    source: "test".into(),
                    limit_bytes: 100,
                    used_bytes: 1,
                    available_bytes: 99,
                }],
            })
        }
    }
    fn gate() -> MemoryAdmission {
        MemoryAdmission::new(LiveSettings::new(Default::default()), Arc::new(Healthy))
    }
    #[test]
    fn projection_preserves_routing_and_validation_without_owning_bulk_fields() {
        let full = json!({"model":"m","input":[{"role":"user","content":"x".repeat(1024*1024)}],"tools":[{"parameters":{"description":"y".repeat(1024*1024)}}],"instructions":"z".repeat(1024*1024),"stream":true,"client_metadata":{"session_id":"session","thread_id":"thread"},"prompt_cache_key":"partition"});
        let raw = serde_json::to_vec(&full).unwrap();
        let small = projection(&raw, &gate()).ok().unwrap();
        assert!(serde_json::to_vec(&small).unwrap().len() < 1024);
        let a = crate::ingress::ResponsesIngress
            .parse(&HeaderMap::new(), full)
            .unwrap();
        let b = crate::ingress::ResponsesIngress
            .parse(&HeaderMap::new(), small)
            .unwrap();
        assert_eq!(a.identity.session_id, b.identity.session_id);
        assert_eq!(a.identity.thread_id, b.identity.thread_id);
        assert_eq!(a.model, b.model);
        assert_eq!(a.stream, b.stream);
        assert_eq!(a.stateless, b.stateless);
        assert_eq!(a.client_origin.source, b.client_origin.source);
    }
    #[test]
    fn malformed_bulk_json_and_identity_conflicts_are_not_hidden() {
        for bytes in [
            br#"{"model":"m","input":[{"text":"bad\q"}]}"#.as_slice(),
            br#"{"model":"m","tools":[1,]}"#,
            br#"[]"#,
            br#"{"input":[]} trailing"#,
        ] {
            assert!(projection(bytes, &gate()).is_err());
        }
        let raw = br#"{"model":"m","input":[],"client_metadata":{"session_id":"b"}}"#;
        let headers = HeaderMap::from_iter([("session-id".parse().unwrap(), "a".parse().unwrap())]);
        assert_eq!(
            crate::ingress::ResponsesIngress
                .parse(&headers, projection(raw, &gate()).ok().unwrap())
                .err()
                .unwrap()
                .code,
            "identity_conflict"
        );
        for raw in [
            br#"{"model":"m","input":{},"unknown":1}"#.as_slice(),
            br#"{"model":"m","input":123}"#,
        ] {
            assert!(
                crate::ingress::ResponsesIngress
                    .parse(&HeaderMap::new(), projection(raw, &gate()).ok().unwrap())
                    .is_err()
            );
        }
    }
}
