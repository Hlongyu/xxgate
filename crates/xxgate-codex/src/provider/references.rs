use serde_json::Value;
use xxgate_core::{Error, Result, identity::IdentityMap};

fn restore_reference(object: &mut Value, field: &str, kind: &str, ids: &IdentityMap) {
    if let Some(value) = object.get(field).and_then(Value::as_str) {
        let original = ids.original_id(kind, value);
        if original != value {
            object[field] = Value::String(original.to_owned());
        }
    }
}

pub(crate) fn rewrite_input(value: &mut Value, ids: &mut IdentityMap) -> Result<()> {
    let Some(items) = value.as_array_mut() else {
        return Ok(());
    };
    for item in items {
        if item.get("type").and_then(Value::as_str) == Some("item_reference") {
            return Err(Error::new(
                409,
                "context_reference_unsupported",
                "Explicit item content is required instead of stored item references",
            ));
        }
        if let Some(content) = item.get("content").and_then(Value::as_array) {
            for part in content {
                if part.get("file_id").is_some() || part.get("image_id").is_some() {
                    return Err(Error::new(
                        409,
                        "image_reference_unsupported",
                        "Provide image bytes or an image URL instead of an account-scoped file reference",
                    ));
                }
            }
        }
        restore_reference(item, "id", "item", ids);
        restore_reference(item, "call_id", "call", ids);
    }
    Ok(())
}

pub(crate) fn filter_event_headers(event: &mut Value) {
    if let Some(response) = event.get_mut("response")
        && let Some(headers) = response.get_mut("headers").and_then(Value::as_object_mut)
    {
        headers.retain(|k, _| {
            ["openai-model", "x-reasoning-included"].contains(&k.to_ascii_lowercase().as_str())
        });
    }
    if let Some(headers) = event.get_mut("headers").and_then(Value::as_object_mut) {
        headers.retain(|k, _| {
            ["openai-model", "x-reasoning-included"].contains(&k.to_ascii_lowercase().as_str())
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use uuid::Uuid;
    use xxgate_core::{
        identity::{Binding, SessionKey},
        types::ModelRef,
    };

    fn ids() -> IdentityMap {
        IdentityMap::new(
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
        )
    }
    #[test]
    fn tool_calls_and_results_keep_original_identifiers_and_content() {
        let mut ids = ids();
        let original = json!([
            {"type":"function_call","id":"fc_original","call_id":"call_original","arguments":"call_original"},
            {"type":"function_call_output","id":"fco_original","call_id":"call_original","output":"done"},
            {"type":"custom_tool_call","id":"ctc_original","call_id":"custom_original","input":"patch"},
            {"type":"custom_tool_call_output","id":"ctco_original","call_id":"custom_original","output":"done"}
        ]);
        let mut input = original.clone();
        rewrite_input(&mut input, &mut ids).unwrap();
        assert_eq!(input, original);
        assert!(ids.take_pending().is_empty());
    }
    #[test]
    fn response_objects_and_delta_references_keep_their_original_ids() {
        let item = json!({"type":"reasoning","id":"rs_upstream","encrypted_content":"PRIVATE_REASONING","summary":[],"internal_chat_message_metadata_passthrough":{"opaque":"PRIVATE_METADATA"}});
        for original in [
            json!({"type":"response.output_item.added","item":{"type":"reasoning","id":"rs_upstream","summary":[]}}),
            json!({"type":"response.reasoning_summary_text.delta","item_id":"rs_upstream","delta":"summary"}),
            json!({"type":"response.output_item.done","item":item}),
            json!({"type":"response.completed","response":{"id":"resp_upstream","output":[item]}}),
        ] {
            let mut event = original.clone();
            filter_event_headers(&mut event);
            assert_eq!(event, original);
        }
    }
    #[test]
    fn encrypted_history_and_bound_ids_pass_through_every_generation() {
        let original = json!([
            {"type":"reasoning","id":"rs_old","encrypted_content":"PRIVATE_OLD_REASONING","summary":[],"internal_chat_message_metadata_passthrough":{"opaque":"PRIVATE_METADATA"}},
            {"type":"compaction","id":"cmp_old","encrypted_content":"PRIVATE_OLD_COMPACTION"},
            {"type":"function_call","id":"fc_old","call_id":"call_old","encrypted_function_args":"PRIVATE_OLD_ARGS","arguments":"{}"},
            {"type":"function_call_output","id":"fco_old","call_id":"call_old","output":"existing result"}
        ]);
        let mut prior = ids();
        // Reproduce the bad outbound mapping already saved by the old gateway.
        assert_ne!(prior.outbound("item", "rs_old").unwrap(), "rs_old");
        let saved = prior.take_pending();
        for generation in [1, 2, 3] {
            let mut ids = IdentityMap::new(prior.binding.clone(), saved.clone());
            ids.binding.generation = generation;
            let mut rewritten = original.clone();
            rewrite_input(&mut rewritten, &mut ids).unwrap();
            assert_eq!(rewritten, original);
            assert!(ids.take_pending().is_empty());
        }
    }
    #[test]
    fn old_client_aliases_are_restored_for_signed_history_and_tool_pairs() {
        let mut previous = ids();
        let item_alias = previous.inbound("item", "rs_upstream").unwrap();
        let call_alias = previous.inbound("call", "call_upstream").unwrap();
        let mappings = previous.take_pending();
        let mut binding = previous.binding.clone();
        binding.id = Uuid::new_v4();
        binding.account_id = Uuid::new_v4();
        binding.namespace = Uuid::new_v4();
        binding.generation = 2;
        let mut ids = IdentityMap::new(binding, vec![]);
        ids.import_legacy_aliases(&previous.binding, &mappings);
        let mut input = json!([
            {"type":"reasoning","id":item_alias,"encrypted_content":"PRIVATE_SIGNED_FOR_rs_upstream"},
            {"type":"function_call","call_id":call_alias,"arguments":"{}"},
            {"type":"function_call_output","call_id":call_alias,"output":"done"}
        ]);
        rewrite_input(&mut input, &mut ids).unwrap();
        assert_eq!(input[0]["id"], "rs_upstream");
        assert_eq!(
            input[0]["encrypted_content"],
            "PRIVATE_SIGNED_FOR_rs_upstream"
        );
        assert_eq!(input[1]["call_id"], "call_upstream");
        assert_eq!(input[2]["call_id"], "call_upstream");
        assert!(ids.take_pending().is_empty());
    }
}
