use crate::wire::{FORMAT, SseDecoder};
use serde_json::{Value, json};
use std::collections::HashSet;
use xxgate_core::{
    Error, Result,
    identity::IdentityMap,
    protocol::{GatewayEvent, Observation, ProtocolDocument, ProviderDecoder},
    usage::Usage,
};

#[derive(Default)]
pub struct CodexDecoder {
    sse: SseDecoder,
    terminal: bool,
    usage: Usage,
    image_ids: HashSet<String>,
    compaction_output: bool,
}

pub(super) fn compact_response(body: &[u8]) -> Result<Observation> {
    let invalid = || {
        Error::new(
            502,
            "invalid_compact_response",
            "Upstream returned an invalid compact response",
        )
    };
    let value: Value = serde_json::from_slice(body).map_err(|_| invalid())?;
    if value["object"] != "response.compaction" {
        return Err(invalid());
    }
    let output = value["output"].as_array().ok_or_else(invalid)?;
    let mut decoder = CodexDecoder::default();
    if let Some(usage) = value.get("usage").filter(|v| v.is_object()) {
        decoder.observe_usage(usage);
    }
    // Output includes retained history. Those old tool calls must not be charged again.
    decoder.usage.source = "upstream_compaction".into();
    decoder.usage.complete =
        decoder.usage.input_tokens.is_some() && decoder.usage.output_tokens.is_some();
    decoder.usage.service_tier = value
        .get("service_tier")
        .and_then(Value::as_str)
        .filter(|tier| ["default", "priority", "fast"].contains(tier))
        .map(str::to_owned);
    Ok(Observation {
        response_model: response_model(&value),
        usage: Some(decoder.usage),
        compaction_output: Some(crate::compaction::has_output(output)),
        terminal: true,
        ..Default::default()
    })
}

impl ProviderDecoder for CodexDecoder {
    fn push(
        &mut self,
        bytes: &[u8],
        _ids: &mut IdentityMap,
        limit: usize,
    ) -> Result<Vec<GatewayEvent>> {
        if self.terminal {
            return Ok(vec![]);
        }
        let mut result = vec![];
        for data in self.sse.push(bytes, limit)? {
            let mut value: Value = serde_json::from_str(&data).map_err(|_| {
                Error::new(
                    502,
                    "invalid_upstream_event",
                    "Upstream SSE event is not valid JSON",
                )
            })?;
            let kind = value
                .get("type")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    Error::new(502, "invalid_upstream_event", "Upstream event has no type")
                })?
                .to_owned();
            let mut observation = Observation {
                recovery_preamble: matches!(
                    kind.as_str(),
                    "response.created" | "response.queued" | "response.in_progress"
                ) && value.get("response").is_some_and(no_reported_work)
                    && value.get("item").is_none()
                    && value.get("delta").is_none(),
                ..Default::default()
            };
            if matches!(kind.as_str(), "response.failed" | "error") {
                let body = value.get("response").unwrap_or(&value);
                if no_reported_work(body)
                    && value.get("item").is_none()
                    && value.get("delta").is_none()
                {
                    observation.encrypted_rejection = super::recovery::rejection(body);
                }
            }
            if kind == "response.output_item.done" && value["item"]["type"] == "compaction" {
                self.compaction_output = true;
            }
            if kind == "codex.rate_limits" {
                observation.quotas = super::quota::windows(&value, "sse");
            }
            if let Some(item) = value.get("item") {
                self.observe_item(item);
            }
            if let Some(response) = value.get("response") {
                if matches!(
                    kind.as_str(),
                    "response.created"
                        | "response.queued"
                        | "response.in_progress"
                        | "response.completed"
                        | "response.failed"
                        | "response.incomplete"
                ) {
                    observation.response_model = response_model(response);
                }
                if let Some(items) = response.get("output").and_then(Value::as_array) {
                    self.compaction_output |= crate::compaction::has_output(items);
                    for item in items {
                        self.observe_item(item);
                    }
                }
                if let Some(tier) = response.get("service_tier").and_then(Value::as_str)
                    && ["default", "priority", "fast"].contains(&tier)
                {
                    self.usage.service_tier = Some(tier.into());
                }
                if let Some(usage) = response.get("usage").filter(|v| v.is_object()) {
                    self.observe_usage(usage);
                }
            }
            observation.content = matches!(
                kind.as_str(),
                "response.output_text.delta"
                    | "response.reasoning_text.delta"
                    | "response.reasoning_summary_text.delta"
                    | "response.function_call_arguments.delta"
                    | "response.custom_tool_call_input.delta"
                    | "response.image_generation_call.partial_image"
            ) && (value
                .get("delta")
                .and_then(Value::as_str)
                .is_some_and(|s| !s.is_empty())
                || value
                    .get("partial_image_b64")
                    .and_then(Value::as_str)
                    .is_some_and(|s| !s.is_empty()));
            if matches!(
                kind.as_str(),
                "response.completed" | "response.failed" | "response.incomplete" | "error"
            ) {
                self.terminal = true;
                observation.terminal = true;
                self.usage.complete =
                    self.usage.input_tokens.is_some() && self.usage.output_tokens.is_some();
                if kind != "response.completed" {
                    let body = value.get("response").unwrap_or(&value);
                    let bytes = serde_json::to_vec(body).map_err(|_| {
                        Error::new(502, "invalid_upstream_event", "Invalid error event")
                    })?;
                    let (error, reason) = super::classify_error(502, &bytes);
                    observation.error = Some(error.clone());
                    observation.disable_reason = reason;
                    if kind == "error" {
                        value = json!({"type":"response.failed","response":{"id":"upstream_error","status":"failed","output":[]}});
                    }
                    if let Some(response) = value.get_mut("response").and_then(Value::as_object_mut)
                    {
                        response.insert("error".into(),json!({"type":"upstream_error","code":error.code,"message":error.client_message()}));
                    }
                }
            }
            self.usage.image_count = self.image_ids.len() as u32;
            observation.compaction_output = if self.compaction_output {
                Some(true)
            } else if kind == "response.completed" {
                Some(false)
            } else {
                None
            };
            observation.usage = Some(self.usage.clone());
            super::references::filter_event_headers(&mut value);
            result.push(GatewayEvent {
                document: ProtocolDocument {
                    format: FORMAT,
                    value,
                },
                observation,
            });
            if self.terminal {
                break;
            }
        }
        Ok(result)
    }
    fn finish(&mut self) -> Result<()> {
        if self.terminal {
            Ok(())
        } else {
            Err(Error::new(
                502,
                "stream_interrupted",
                "Upstream stream ended before a terminal response",
            ))
        }
    }
    fn buffered_bytes(&self) -> usize {
        self.sse.buffered_bytes()
    }
}

fn no_reported_work(value: &Value) -> bool {
    fn no_usage(value: &Value) -> bool {
        match value {
            Value::Null => true,
            Value::Number(n) => n.as_u64() == Some(0),
            Value::Object(o) => o.values().all(no_usage),
            _ => false,
        }
    }
    value.is_object()
        && value
            .get("output")
            .is_none_or(|output| output.is_null() || output.as_array().is_some_and(Vec::is_empty))
        && value.get("usage").is_none_or(no_usage)
}

impl CodexDecoder {
    fn observe_usage(&mut self, value: &Value) {
        // Keep numeric usage facts, including future token breakdowns. Free text
        // and arbitrary provider payloads are never admitted to persistence.
        self.usage.raw_usage = numeric_usage(value, 0).unwrap_or(Value::Null);
        self.usage.input_tokens = value
            .get("input_tokens")
            .and_then(Value::as_u64)
            .or(self.usage.input_tokens);
        self.usage.output_tokens = value
            .get("output_tokens")
            .and_then(Value::as_u64)
            .or(self.usage.output_tokens);
        self.usage.cached_input_tokens = value
            .pointer("/input_tokens_details/cached_tokens")
            .and_then(Value::as_u64)
            .or(self.usage.cached_input_tokens);
        self.usage.reasoning_output_tokens = value
            .pointer("/output_tokens_details/reasoning_tokens")
            .and_then(Value::as_u64)
            .or(self.usage.reasoning_output_tokens);
        self.usage.source = "upstream_response".into();
    }
    fn observe_item(&mut self, item: &Value) {
        if item.get("type").and_then(Value::as_str) != Some("image_generation_call") {
            return;
        }
        if item
            .get("result")
            .and_then(Value::as_str)
            .is_none_or(|s| s.is_empty())
        {
            return;
        }
        let Some(id) = item.get("id").and_then(Value::as_str) else {
            return;
        };
        if !self.image_ids.insert(id.into()) {
            return;
        }
        if let Some(usage) = item.get("usage") {
            self.usage.image_tool_usage_reported = true;
            if let Some(input) = usage.get("input_tokens").and_then(Value::as_u64) {
                self.usage.image_input_tokens = Some(
                    self.usage
                        .image_input_tokens
                        .unwrap_or(0)
                        .saturating_add(input),
                );
            }
            if let Some(output) = usage.get("output_tokens").and_then(Value::as_u64) {
                self.usage.image_output_tokens = Some(
                    self.usage
                        .image_output_tokens
                        .unwrap_or(0)
                        .saturating_add(output),
                );
            }
        }
    }
}

fn response_model(response: &Value) -> Option<String> {
    response
        .get("model")
        .and_then(Value::as_str)
        .filter(|name| {
            !name.is_empty()
                && name.len() <= 160
                && name
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"-._:/".contains(&c))
        })
        .map(str::to_owned)
}

fn numeric_usage(value: &Value, depth: usize) -> Option<Value> {
    if depth > 6 {
        return None;
    }
    match value {
        Value::Number(_) | Value::Bool(_) => Some(value.clone()),
        Value::Object(object) => Some(Value::Object(
            object
                .iter()
                .take(64)
                .filter(|(k, _)| {
                    k.len() <= 80 && k.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_')
                })
                .filter_map(|(k, v)| numeric_usage(v, depth + 1).map(|v| (k.clone(), v)))
                .collect(),
        )),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;
    use xxgate_core::{
        identity::{Binding, SessionKey},
        types::ModelRef,
    };
    #[test]
    fn response_models_are_bounded_metadata_from_response_objects_only() {
        let mut ids = IdentityMap::new(
            Binding::new(
                SessionKey {
                    key_id: Uuid::new_v4(),
                    client_session_id: "s".into(),
                },
                Uuid::new_v4(),
                &ModelRef::codex("m"),
                1,
            ),
            vec![],
        );
        for kind in [
            "response.created",
            "response.in_progress",
            "response.completed",
            "response.failed",
            "response.incomplete",
        ] {
            let mut decoder = CodexDecoder::default();
            let input =
                json!({"type":kind,"response":{"model":"gpt-returned-2026-09-17","output":[]}});
            let events = decoder
                .push(format!("data: {input}\n\n").as_bytes(), &mut ids, 4096)
                .unwrap();
            assert_eq!(
                events[0].observation.response_model.as_deref(),
                Some("gpt-returned-2026-09-17")
            );
            assert_eq!(
                events[0].document.value["response"]["model"],
                input["response"]["model"]
            );
        }
        for model in [
            Value::Null,
            json!(42),
            json!({"name":"m"}),
            json!(""),
            json!("x".repeat(161)),
            json!("model\nsecret"),
            json!("<script>"),
            json!("PRIVATE MODEL TEXT"),
        ] {
            let mut decoder = CodexDecoder::default();
            let input = json!({"type":"response.completed","response":{"model":model,"output":[]}});
            let events = decoder
                .push(format!("data: {input}\n\n").as_bytes(), &mut ids, 4096)
                .unwrap();
            assert!(events[0].observation.response_model.is_none());
            assert_eq!(events[0].document.value["response"]["model"], model);
        }
        let mut decoder = CodexDecoder::default();
        for input in [
            json!({"type":"response.output_item.done","item":{"model":"tool-model"},"response":{"model":"nested-model"}}),
            json!({"type":"response.completed","model":"top-level-event-model","response":{"output":[{"model":"tool-model"}]}}),
        ] {
            let events = decoder
                .push(format!("data: {input}\n\n").as_bytes(), &mut ids, 4096)
                .unwrap();
            assert!(events[0].observation.response_model.is_none());
        }
        for model in [Some("ft:gpt-model:org:custom"), None] {
            let value = json!({"object":"response.compaction","model":model,"output":[{"model":"history-model"}]});
            let observation = compact_response(&serde_json::to_vec(&value).unwrap()).unwrap();
            assert_eq!(observation.response_model.as_deref(), model);
        }
    }
    #[test]
    fn cumulative_usage_is_replaced_and_events_preserve_response_ids() {
        let mut decoder = CodexDecoder::default();
        let mut ids = IdentityMap::new(
            Binding::new(
                SessionKey {
                    key_id: Uuid::new_v4(),
                    client_session_id: "s".into(),
                },
                Uuid::new_v4(),
                &ModelRef::codex("m"),
                1,
            ),
            vec![],
        );
        let a=decoder.push(b"data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_original\",\"usage\":{\"input_tokens\":10,\"output_tokens\":0}}}\n\n",&mut ids,4096).unwrap();
        let b=decoder.push(b"data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_original\",\"service_tier\":\"default\",\"usage\":{\"input_tokens\":10,\"output_tokens\":20},\"output\":[]}}\n\n",&mut ids,4096).unwrap();
        assert_eq!(
            a[0].document.value["response"]["id"],
            b[0].document.value["response"]["id"]
        );
        assert_eq!(
            b[0].observation.usage.as_ref().unwrap().input_tokens,
            Some(10)
        );
        assert!(b[0].observation.terminal);
        assert!(decoder.finish().is_ok());
        assert_eq!(b[0].document.value["response"]["id"], "resp_original");
        assert!(ids.take_pending().is_empty());
    }
}
