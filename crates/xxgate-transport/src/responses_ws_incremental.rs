//! Connection-local continuation proofs. No message bodies or fingerprints are
//! persisted or logged. A fresh secret prevents cross-connection correlation.
use hmac::{Hmac, Mac};
use serde_json::Value;
use sha2::Sha256;
use std::{collections::BTreeMap, io};
use uuid::Uuid;
use xxgate_core::{Error, Result};

type Hash = Hmac<Sha256>;

struct Pending {
    input: Hash,
    items: usize,
    config: [u8; 32],
}

struct Previous {
    id: String,
    prefix: [u8; 32],
    items: usize,
    config: [u8; 32],
}

pub(crate) struct Continuation {
    secret: [u8; 16],
    previous: Option<Previous>,
    pending: Option<Pending>,
}

impl Default for Continuation {
    fn default() -> Self {
        Self {
            secret: *Uuid::new_v4().as_bytes(),
            previous: None,
            pending: None,
        }
    }
}

#[derive(Default)]
pub(crate) struct Plan {
    pub input_items: usize,
    pub skipped_items: usize,
}

impl Continuation {
    fn hash(&self) -> Hash {
        Hash::new_from_slice(&self.secret).expect("HMAC accepts a 16-byte key")
    }

    pub fn prepare(&mut self, body: &mut Value, enabled: bool) -> Result<Plan> {
        self.pending = None;
        let previous = self.previous.take();
        let Some(input) = body.get("input").and_then(Value::as_array) else {
            return Ok(Plan::default());
        };
        let mut plan = Plan {
            input_items: input.len(),
            skipped_items: 0,
        };
        if !enabled || has_compaction(input) || compaction_metadata(body) {
            return Ok(plan);
        }
        // Every generation-affecting option must match, including unknown new
        // options. Per-turn tracing metadata is deliberately not conversation.
        let config: BTreeMap<_, _> = body
            .as_object()
            .ok_or_else(invalid)?
            .iter()
            .filter(|(name, _)| {
                !matches!(
                    name.as_str(),
                    "input" | "client_metadata" | "previous_response_id"
                )
            })
            .collect();
        let config = digest(self.hash(), &config)?;
        let previous = previous.filter(|p| p.config == config && input.len() > p.items);
        let mut hash = self.hash();
        let mut matched = false;
        for (index, item) in input.iter().enumerate() {
            hash.update(&digest(self.hash(), item)?);
            if let Some(p) = &previous
                && index + 1 == p.items
            {
                matched = <[u8; 32]>::from(hash.clone().finalize().into_bytes()) == p.prefix;
            }
        }
        self.pending = Some(Pending {
            input: hash,
            items: input.len(),
            config,
        });
        if let Some(p) = previous.filter(|_| matched) {
            plan.skipped_items = p.items;
            let body = body.as_object_mut().ok_or_else(invalid)?;
            body.get_mut("input")
                .and_then(Value::as_array_mut)
                .ok_or_else(invalid)?
                .drain(..p.items);
            body.insert("previous_response_id".into(), Value::String(p.id));
        }
        Ok(plan)
    }

    pub fn observe(&mut self, event: &Value, enabled: bool) -> Result<()> {
        if !enabled {
            self.pending = None;
            self.previous = None;
            return Ok(());
        }
        let kind = event.get("type").and_then(Value::as_str);
        if kind == Some("response.output_item.done") && event.get("item").is_some_and(is_compaction)
        {
            self.pending = None;
        }
        if matches!(
            kind,
            Some("error" | "response.failed" | "response.incomplete")
        ) {
            self.pending = None;
            self.previous = None;
        }
        if kind != Some("response.completed") {
            return Ok(());
        }
        let Some(mut pending) = self.pending.take() else {
            return Ok(());
        };
        let Some(response) = event.get("response") else {
            return Ok(());
        };
        if response.get("status").and_then(Value::as_str) != Some("completed") {
            return Ok(());
        }
        let Some(id) = response
            .get("id")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty() && s.len() <= 512)
        else {
            return Ok(());
        };
        let Some(output) = response.get("output").and_then(Value::as_array) else {
            return Ok(());
        };
        if has_compaction(output) {
            return Ok(());
        }
        for item in output {
            pending.input.update(&digest(self.hash(), item)?);
        }
        self.previous = Some(Previous {
            id: id.to_owned(),
            prefix: pending.input.finalize().into_bytes().into(),
            items: pending.items + output.len(),
            config: pending.config,
        });
        Ok(())
    }
}

fn invalid() -> Error {
    Error::invalid("Invalid incremental Responses payload")
}

struct HashWriter(Hash);
impl io::Write for HashWriter {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.0.update(data);
        Ok(data.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn digest(hash: Hash, value: &impl serde::Serialize) -> Result<[u8; 32]> {
    let mut writer = HashWriter(hash);
    serde_json::to_writer(&mut writer, value).map_err(|_| invalid())?;
    Ok(writer.0.finalize().into_bytes().into())
}

fn is_compaction(item: &Value) -> bool {
    matches!(
        item.get("type").and_then(Value::as_str),
        Some("compaction" | "compaction_trigger")
    )
}
fn has_compaction(items: &[Value]) -> bool {
    items.iter().any(is_compaction)
}
fn compaction_metadata(body: &Value) -> bool {
    let metadata = &body["client_metadata"];
    metadata["request_kind"] == "compaction"
        || metadata
            .get("x-codex-turn-metadata")
            .and_then(Value::as_str)
            .and_then(|s| serde_json::from_str::<Value>(s).ok())
            .is_some_and(|v| v["request_kind"] == "compaction")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn request(input: Value) -> Value {
        json!({"type":"response.create","model":"test","instructions":"Keep all context","input":input,"tools":[],"reasoning":{"effort":"low"},"store":false})
    }
    fn complete(state: &mut Continuation, id: &str, output: Value) {
        state.observe(&json!({"type":"response.completed","response":{"id":id,"status":"completed","output":output}}), true).unwrap();
    }
    fn assistant() -> Value {
        json!({"type":"message","role":"assistant","id":"msg_1","status":"completed","phase":"final_answer","content":[{"type":"output_text","text":"Answer","annotations":[]}]})
    }

    #[test]
    fn chains_only_after_complete_input_and_all_output_match() {
        let mut state = Continuation::default();
        let first = json!({"role":"user","content":"Remember the value 42"});
        let reasoning =
            json!({"type":"reasoning","id":"rs_1","encrypted_content":"OPAQUE","summary":[]});
        let output = assistant();
        let mut body = request(json!([first]));
        assert_eq!(state.prepare(&mut body, true).unwrap().skipped_items, 0);
        complete(&mut state, "resp_1", json!([reasoning, output]));
        let next = json!({"role":"user","content":"What was the value?"});
        let mut body = request(json!([first, reasoning, output, next]));
        body["client_metadata"] = json!({"turn_id":"two"});
        let plan = state.prepare(&mut body, true).unwrap();
        assert_eq!(plan.skipped_items, 3);
        assert_eq!(body["previous_response_id"], "resp_1");
        assert_eq!(body["input"], json!([next]));
        complete(&mut state, "resp_2", json!([output]));
        let mut body = request(
            json!([first, reasoning, output, next, output, {"role":"user","content":"Continue"}]),
        );
        assert_eq!(state.prepare(&mut body, true).unwrap().skipped_items, 5);
        assert_eq!(body["previous_response_id"], "resp_2");
        assert_eq!(body["input"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn tool_results_are_new_input_and_call_identity_must_match() {
        let first = json!({"role":"user","content":"Run a test"});
        let call = json!({"type":"function_call","id":"fc_1","call_id":"call_1","name":"test","arguments":"{}","status":"completed"});
        let result = json!({"type":"function_call_output","call_id":"call_1","output":"passed"});
        let mut state = Continuation::default();
        state.prepare(&mut request(json!([first])), true).unwrap();
        complete(&mut state, "resp_tools", json!([call]));
        let mut body = request(json!([first, call, result]));
        assert_eq!(state.prepare(&mut body, true).unwrap().skipped_items, 2);
        assert_eq!(body["input"], json!([result]));
        let mut state = Continuation::default();
        state.prepare(&mut request(json!([first])), true).unwrap();
        complete(&mut state, "resp_tools", json!([call]));
        let mut modified_call = call;
        modified_call["call_id"] = json!("different");
        let mut body = request(json!([first, modified_call, result]));
        assert_eq!(state.prepare(&mut body, true).unwrap().skipped_items, 0);
    }

    #[test]
    fn changed_truncated_or_rewritten_history_always_sends_full_input() {
        let first = json!({"role":"user","content":"Original"});
        let output = assistant();
        for input in [
            json!([{"role":"user","content":"Edited"},output,{"role":"user","content":"Next"}]),
            json!([first,{"role":"assistant","content":"Answer"},{"role":"user","content":"Next"}]),
            json!([first,{"role":"user","content":"Missing output"}]),
            json!([first, output]), // No new items: not a continuation.
            json!([first]),         // Repeating the original request must generate afresh.
        ] {
            let mut state = Continuation::default();
            state.prepare(&mut request(json!([first])), true).unwrap();
            complete(&mut state, "resp_1", json!([output]));
            let mut body = request(input.clone());
            assert_eq!(state.prepare(&mut body, true).unwrap().skipped_items, 0);
            assert_eq!(body["input"], input);
            assert!(body.get("previous_response_id").is_none());
        }
    }

    #[test]
    fn generation_options_and_opaque_fields_invalidate_continuation() {
        let first = json!({"role":"user","content":"Original"});
        let output = assistant();
        for (field, value) in [
            ("model", json!("other")),
            ("instructions", json!("Changed")),
            ("tools", json!([{"type":"function","name":"other"}])),
            ("tool_choice", json!("none")),
            ("reasoning", json!({"effort":"high"})),
            ("service_tier", json!("priority")),
            ("prompt_cache_key", json!("other")),
            ("access_programs", json!({"cyber":"standard"})),
            ("future_option", json!(true)),
        ] {
            let mut state = Continuation::default();
            state.prepare(&mut request(json!([first])), true).unwrap();
            complete(&mut state, "resp_1", json!([output]));
            let mut body = request(json!([first,output,{"role":"user","content":"Next"}]));
            body[field] = value;
            assert_eq!(
                state.prepare(&mut body, true).unwrap().skipped_items,
                0,
                "{field}"
            );
        }
        let mut state = Continuation::default();
        state.prepare(&mut request(json!([first])), true).unwrap();
        let mut with_metadata = output.clone();
        with_metadata["opaque"] = json!("signed-data");
        complete(&mut state, "resp_1", json!([with_metadata]));
        assert_eq!(
            state
                .prepare(
                    &mut request(json!([first, output, {"role":"user","content":"Next"}])),
                    true
                )
                .unwrap()
                .skipped_items,
            0
        );
    }

    #[test]
    fn failures_missing_completion_compaction_and_disabled_mode_do_not_chain() {
        let first = json!({"role":"user","content":"Original"});
        let output = assistant();
        for event in [
            json!({"type":"error","error":{"code":"previous_response_not_found"}}),
            json!({"type":"response.failed"}),
            json!({"type":"response.incomplete"}),
            json!({"type":"response.created","response":{"id":"resp_not_finished"}}),
            json!({"type":"response.completed","response":{"id":"resp_1","status":"completed"}}),
        ] {
            let mut state = Continuation::default();
            state.prepare(&mut request(json!([first])), true).unwrap();
            state.observe(&event, true).unwrap();
            assert_eq!(
                state
                    .prepare(
                        &mut request(json!([first,output,{"role":"user","content":"Next"}])),
                        true
                    )
                    .unwrap()
                    .skipped_items,
                0
            );
        }
        for input in [
            json!([first,{"type":"compaction_trigger"}]),
            json!([{"type":"compaction","encrypted_content":"OPAQUE"},first]),
        ] {
            let mut state = Continuation::default();
            state.prepare(&mut request(input.clone()), true).unwrap();
            complete(&mut state, "resp_compacted", json!([output]));
            assert!(state.previous.is_none());
        }
        let mut state = Continuation::default();
        state.prepare(&mut request(json!([first])), true).unwrap();
        complete(&mut state, "resp_1", json!([output]));
        let full = request(json!([first,output,{"role":"user","content":"Next"}]));
        assert_eq!(
            state
                .prepare(&mut full.clone(), false)
                .unwrap()
                .skipped_items,
            0
        );
        assert_eq!(
            state
                .prepare(&mut full.clone(), true)
                .unwrap()
                .skipped_items,
            0
        );
        let mut compact = request(json!([first]));
        compact["client_metadata"] =
            json!({"x-codex-turn-metadata":"{\"request_kind\":\"compaction\"}"});
        state.prepare(&mut compact, true).unwrap();
        complete(&mut state, "resp_compacted", json!([output]));
        assert!(state.previous.is_none());
    }
}
