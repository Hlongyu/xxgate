//! JSON construction checkpoints measured pressure before it can grow an
//! unbounded Value tree from a compact body. This is not a byte reservation.
use crate::memory::{MemoryAdmission, MemoryRejection};
use serde::de::{self, DeserializeSeed, MapAccess, SeqAccess, Visitor};
use serde_json::Value;
use std::fmt;

pub enum ParseError {
    Invalid,
    Memory(Box<MemoryRejection>),
}
struct Context<'a> {
    gate: &'a MemoryAdmission,
    nodes: usize,
    rejection: Option<Box<MemoryRejection>>,
}
impl Context<'_> {
    fn check<E: de::Error>(&mut self, force: bool) -> Result<(), E> {
        self.nodes += 1;
        if (force || self.nodes.is_multiple_of(4096))
            && let Err(error) = self.gate.check(true)
        {
            self.rejection = Some(error);
            return Err(E::custom("memory admission rejected JSON construction"));
        }
        Ok(())
    }
}
struct Seed<'a, 'b>(&'a mut Context<'b>);
impl<'de> DeserializeSeed<'de> for Seed<'_, '_> {
    type Value = Value;
    fn deserialize<D: de::Deserializer<'de>>(self, d: D) -> Result<Value, D::Error> {
        self.0.check(false)?;
        d.deserialize_any(self)
    }
}
impl<'de> Visitor<'de> for Seed<'_, '_> {
    type Value = Value;
    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("a JSON value")
    }
    fn visit_unit<E: de::Error>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }
    fn visit_bool<E: de::Error>(self, v: bool) -> Result<Value, E> {
        Ok(Value::Bool(v))
    }
    fn visit_i64<E: de::Error>(self, v: i64) -> Result<Value, E> {
        Ok(Value::from(v))
    }
    fn visit_u64<E: de::Error>(self, v: u64) -> Result<Value, E> {
        Ok(Value::from(v))
    }
    fn visit_f64<E: de::Error>(self, v: f64) -> Result<Value, E> {
        serde_json::Number::from_f64(v)
            .map(Value::Number)
            .ok_or_else(|| E::custom("invalid number"))
    }
    fn visit_str<E: de::Error>(self, v: &str) -> Result<Value, E> {
        if v.len() > 1024 * 1024 {
            self.0.check(true)?;
        }
        Ok(Value::String(v.to_owned()))
    }
    fn visit_string<E: de::Error>(self, v: String) -> Result<Value, E> {
        if v.len() > 1024 * 1024 {
            self.0.check(true)?;
        }
        Ok(Value::String(v))
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Value, A::Error> {
        let mut values = Vec::new();
        while let Some(v) = seq.next_element_seed(Seed(self.0))? {
            values.push(v);
        }
        Ok(Value::Array(values))
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Value, A::Error> {
        let mut values = serde_json::Map::new();
        while let Some(key) = map.next_key::<String>()? {
            values.insert(key, map.next_value_seed(Seed(self.0))?);
        }
        Ok(Value::Object(values))
    }
}
pub fn parse(bytes: &[u8], gate: &MemoryAdmission) -> Result<Value, ParseError> {
    gate.check(true).map_err(ParseError::Memory)?;
    let mut context = Context {
        gate,
        nodes: 0,
        rejection: None,
    };
    let mut decoder = serde_json::Deserializer::from_slice(bytes);
    let result = Seed(&mut context)
        .deserialize(&mut decoder)
        .and_then(|value| decoder.end().map(|_| value));
    match result {
        Ok(value) => Ok(value),
        Err(_) => Err(context
            .rejection
            .map(ParseError::Memory)
            .unwrap_or(ParseError::Invalid)),
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        memory::{MemoryDomain, MemoryProbe, MemorySample},
        settings::{LiveSettings, RuntimeSettings},
    };
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    struct Probe {
        reads: AtomicUsize,
        fail_after: usize,
    }
    impl MemoryProbe for Probe {
        fn sample(&self) -> Result<MemorySample, &'static str> {
            let used = if self.reads.fetch_add(1, Ordering::SeqCst) >= self.fail_after {
                95
            } else {
                20
            };
            Ok(MemorySample {
                domains: vec![MemoryDomain {
                    source: "host".into(),
                    limit_bytes: 100,
                    used_bytes: used,
                    available_bytes: 100 - used,
                }],
            })
        }
    }
    fn gate(fail_after: usize) -> MemoryAdmission {
        MemoryAdmission::new(
            LiveSettings::new(RuntimeSettings::default()),
            Arc::new(Probe {
                reads: AtomicUsize::new(0),
                fail_after,
            }),
        )
    }
    #[test]
    fn preserves_json_semantics_and_recursion_limit() {
        let bytes =
            br#"{"a":[null,true,-4,18446744073709551615,1.25,"unicode \u4f60"],"dup":1,"dup":2}"#;
        assert_eq!(
            parse(bytes, &gate(usize::MAX)).ok().unwrap(),
            serde_json::from_slice::<Value>(bytes).unwrap()
        );
        for bad in [b"{} {}".as_slice(), b"{invalid}"] {
            assert!(matches!(
                parse(bad, &gate(usize::MAX)),
                Err(ParseError::Invalid)
            ));
        }
        let deep = format!("{}0{}", "[".repeat(200), "]".repeat(200));
        assert!(matches!(
            parse(deep.as_bytes(), &gate(usize::MAX)),
            Err(ParseError::Invalid)
        ));
    }
    #[test]
    fn pressure_is_checked_while_constructing_a_large_tree() {
        let body = serde_json::to_vec(&vec![0; 20000]).unwrap();
        assert!(matches!(parse(&body, &gate(1)), Err(ParseError::Memory(_))));
    }
}
