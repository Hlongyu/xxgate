use serde_json::{Map, Value, json};

/// Codex OAuth rejects replayed hosted search history without a search tool
/// declaration (including local summary requests whose tools array is empty).
/// Keep the history intact and preserve the caller's no-tool execution intent.
pub(super) fn ensure_declaration(body: &mut Map<String, Value>) {
    let Some(input) = body.get("input").and_then(Value::as_array) else {
        return;
    };
    if !input.iter().any(|item| item["type"] == "web_search_call") {
        return;
    }
    let additional = input
        .iter()
        .filter(|item| item["type"] == "additional_tools")
        .filter_map(|item| item.get("tools").and_then(Value::as_array));
    let declared: Vec<&Value> = body
        .get("tools")
        .and_then(Value::as_array)
        .into_iter()
        .chain(additional)
        .flatten()
        .collect();
    if declared.iter().any(|tool| {
        tool.get("type")
            .and_then(Value::as_str)
            .is_some_and(|kind| kind == "web_search" || kind.starts_with("web_search_preview"))
    }) {
        return;
    }
    let no_tools = declared.is_empty();
    // Do not reinterpret malformed tools; validation owns those errors.
    if body
        .get("tools")
        .is_some_and(|v| !v.is_array() && !v.is_null())
    {
        return;
    }
    let tool = json!({"type":"web_search", "external_web_access":false});
    body.entry("tools").or_insert_with(|| json!([]));
    if body["tools"].is_null() {
        body.insert("tools".into(), json!([]));
    }
    body.get_mut("tools")
        .unwrap()
        .as_array_mut()
        .unwrap()
        .push(tool);
    if no_tools
        && (body.get("tool_choice").is_none()
            || matches!(
                body.get("tool_choice").and_then(Value::as_str),
                Some("auto" | "none")
            ))
    {
        body.insert("tool_choice".into(), json!("none"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary_and_v2_keep_history_and_cannot_call_injected_search() {
        for tools in [None, Some(json!([])), Some(Value::Null)] {
            for choice in [None, Some(json!("auto")), Some(json!("none"))] {
                for v2 in [false, true] {
                    let mut body = json!({"input":[
                        {"type":"web_search_call","id":"ws_original","action":{"type":"search","query":"example"}},
                        {"role":"user","content":"Summarize this conversation"}
                    ]});
                    if v2 {
                        body["input"]
                            .as_array_mut()
                            .unwrap()
                            .push(json!({"type":"compaction_trigger"}));
                    }
                    if let Some(tools) = &tools {
                        body["tools"] = tools.clone();
                    }
                    if let Some(choice) = &choice {
                        body["tool_choice"] = choice.clone();
                    }
                    let input = body["input"].clone();
                    ensure_declaration(body.as_object_mut().unwrap());
                    assert_eq!(body["input"], input);
                    assert_eq!(
                        body["tools"],
                        json!([{"type":"web_search","external_web_access":false}])
                    );
                    assert_eq!(body["tool_choice"], "none");
                    let once = body.clone();
                    ensure_declaration(body.as_object_mut().unwrap());
                    assert_eq!(body, once);
                }
            }
        }
    }

    #[test]
    fn existing_search_and_unrelated_requests_are_unchanged() {
        for mut body in [
            json!({"input":[{"role":"user","content":"web_search_call"}],"tools":[]}),
            json!({"input":"web_search_call"}),
            json!({"input":[{"type":"web_search_call"}],"tools":[{"type":"web_search","external_web_access":true}]}),
            json!({"input":[{"type":"web_search_call"}],"tools":[{"type":"web_search_preview"}]}),
            json!({"input":[{"type":"web_search_call"},{"type":"additional_tools","tools":[{"type":"web_search"}]}],"tools":[]}),
        ] {
            let before = body.clone();
            ensure_declaration(body.as_object_mut().unwrap());
            assert_eq!(body, before);
        }
    }

    #[test]
    fn function_tools_and_explicit_choices_are_preserved() {
        for choice in [
            json!("auto"),
            json!("required"),
            json!({"type":"function","name":"work"}),
        ] {
            let tool = json!({"type":"function","name":"work","parameters":{"type":"object"}});
            let mut body =
                json!({"input":[{"type":"web_search_call"}],"tools":[tool],"tool_choice":choice});
            ensure_declaration(body.as_object_mut().unwrap());
            assert_eq!(body["tools"][0], tool);
            assert_eq!(body["tools"].as_array().unwrap().len(), 2);
            assert_eq!(body["tool_choice"], choice);
        }
    }
}
