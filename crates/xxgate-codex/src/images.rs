//! Standalone JSON Images requests used by Codex's image_gen tool.
use bytes::Bytes;
use http::{HeaderMap, HeaderValue, Method};
use serde_json::{Value, json};
use xxgate_core::{
    Error, Result,
    accounts::{Account, Credentials},
    identity::IdentityMap,
    protocol::{GatewayRequest, PreparedRequest, RequestKind},
    providers::{ModelSpec, is_codex_image_model},
    usage::Usage,
};

pub const FORMAT: &str = "codex.images.v1";
const TURN_HEADER: &str = "x-codex-image-turn-id";

pub fn parse(headers: &HeaderMap, body: Value, kind: RequestKind) -> Result<GatewayRequest> {
    if !kind.is_image() || !body.is_object() {
        return Err(Error::invalid("Images request must be a JSON object"));
    }
    let allowed = [
        "model",
        "prompt",
        "images",
        "mask",
        "background",
        "n",
        "quality",
        "size",
        "output_format",
        "output_compression",
        "moderation",
        "stream",
        "client_metadata",
    ];
    if body
        .as_object()
        .unwrap()
        .keys()
        .any(|k| !allowed.contains(&k.as_str()))
    {
        return Err(Error::new(
            400,
            "unsupported_parameter",
            "Unsupported Images parameter",
        ));
    }
    if body.get("stream").is_some_and(|v| v != &json!(false)) {
        return Err(Error::invalid(
            "Codex Images returns JSON; streaming is not supported",
        ));
    }
    if body
        .get("prompt")
        .and_then(Value::as_str)
        .is_none_or(|p| p.trim().is_empty())
    {
        return Err(Error::invalid("Images requires a nonempty prompt"));
    }
    if body
        .get("n")
        .is_some_and(|v| !v.as_u64().is_some_and(|n| (1..=10).contains(&n)))
    {
        return Err(Error::invalid("n must be an integer between 1 and 10"));
    }
    if kind == RequestKind::ImageEdit {
        let images = body
            .get("images")
            .and_then(Value::as_array)
            .filter(|images| !images.is_empty() && images.len() <= 16)
            .ok_or_else(|| Error::invalid("Image edits require 1 to 16 images with image_url"))?;
        for image in images {
            validate_image(image)?;
        }
        if let Some(mask) = body.get("mask") {
            validate_image(mask)?;
        }
    } else if body.get("images").is_some() || body.get("mask").is_some() {
        return Err(Error::invalid("Use /images/edits with input images"));
    }
    // Reuse the gateway's conflict checks and stateless/session isolation, not
    // Responses' content schema. Prompts and image bytes never become metadata.
    let mut projection = json!({"model":body.get("model"),"input":""});
    if let Some(meta) = body.get("client_metadata") {
        projection["client_metadata"] = meta.clone();
    }
    let mut request = crate::ingress::parse(headers, projection, kind)?;
    let turn = crate::ingress::coherent(headers, Some(TURN_HEADER), &[], "turn_id")?;
    if headers.get_all(TURN_HEADER).iter().count() > 1 {
        return Err(Error::invalid("Duplicate image turn headers"));
    }
    if let Some(turn) = turn {
        request.identity_headers.insert(
            TURN_HEADER,
            HeaderValue::from_str(&turn)
                .map_err(|_| Error::invalid("Invalid image turn header"))?,
        );
        request
            .identifier_inputs
            .insert(format!("headers.{TURN_HEADER}"), turn);
    }
    request.client_origin = crate::client_source::detect(headers, &body);
    request.document.format = FORMAT;
    request.document.value = body;
    Ok(request)
}

fn validate_image(value: &Value) -> Result<()> {
    let valid = value
        .get("image_url")
        .and_then(Value::as_str)
        .is_some_and(|s| {
            !s.is_empty()
                && (s.starts_with("data:image/")
                    || s.starts_with("https://")
                    || s.starts_with("http://"))
        });
    if !valid {
        return Err(Error::invalid(
            "Provide an image_url containing image bytes or an HTTP(S) URL",
        ));
    }
    Ok(())
}

pub(crate) fn validate(request: &GatewayRequest, model: &ModelSpec) -> Result<()> {
    if request.document.format != FORMAT || !is_codex_image_model(&model.upstream) {
        return Err(Error::invalid(
            "This route requires a supported Codex Images model",
        ));
    }
    if !model.capabilities.image_generation
        || (request.kind == RequestKind::ImageEdit && !model.capabilities.image_input)
    {
        return Err(Error::invalid(
            "Image generation or editing is disabled for this model",
        ));
    }
    Ok(())
}

pub(crate) fn prepare(
    r: &GatewayRequest,
    model: &ModelSpec,
    account: &Account,
    credentials: &Credentials,
    ids: &mut IdentityMap,
) -> Result<PreparedRequest> {
    let mut body = r.document.value.clone();
    body["model"] = json!(model.upstream.model);
    // Metadata is carried by normalized headers; it is not an Images field.
    body.as_object_mut().unwrap().remove("client_metadata");
    body.as_object_mut().unwrap().remove("stream");
    let (mut headers, _) =
        crate::provider::request::normalized_headers(r, account, credentials, ids)?;
    headers.insert("accept", HeaderValue::from_static("application/json"));
    let operation = if r.kind == RequestKind::ImageEdit {
        "edits"
    } else {
        "generations"
    };
    Ok(PreparedRequest {
        method: Method::POST,
        url: format!(
            "{}/images/{operation}",
            account.upstream_base_url.trim_end_matches('/')
        ),
        headers,
        body: Bytes::from(
            serde_json::to_vec(&body).map_err(|_| Error::invalid("Invalid Images body"))?,
        ),
        account_id: Some(account.id),
        profile_version: account.version,
        tls_backend: account.profile.tls_backend.clone(),
    })
}

pub(crate) fn response(body: &[u8]) -> Result<Usage> {
    let invalid = || {
        Error::new(
            502,
            "invalid_image_response",
            "Upstream returned an invalid Images response",
        )
    };
    let value: Value = serde_json::from_slice(body).map_err(|_| invalid())?;
    let data = value
        .get("data")
        .and_then(Value::as_array)
        .filter(|data| !data.is_empty() && data.len() <= 10)
        .ok_or_else(invalid)?;
    if !data.iter().all(|image| {
        image
            .get("b64_json")
            .and_then(Value::as_str)
            .is_some_and(|s| !s.is_empty())
    }) {
        return Err(invalid());
    }
    let upstream = &value["usage"];
    // Whitelist numeric usage facts. Never persist b64_json, revised_prompt,
    // safety_identifier or unrecognized/free-text fields from the upstream.
    let mut raw = json!({});
    for key in ["input_tokens", "output_tokens", "total_tokens"] {
        if let Some(n) = upstream[key].as_u64() {
            raw[key] = json!(n);
        }
    }
    for key in ["input_tokens_details", "output_tokens_details"] {
        for field in ["text_tokens", "image_tokens"] {
            if let Some(n) = upstream[key][field].as_u64() {
                raw[key][field] = json!(n);
            }
        }
    }
    let input = upstream["input_tokens"].as_u64();
    let output = upstream["output_tokens"].as_u64();
    let image_input = upstream["input_tokens_details"]["image_tokens"].as_u64();
    let image_output = upstream["output_tokens_details"]["image_tokens"].as_u64();
    Ok(Usage {
        input_tokens: input,
        output_tokens: output,
        // Direct Images API does not use cached input billing.
        cached_input_tokens: Some(0),
        image_input_tokens: image_input,
        image_output_tokens: image_output,
        image_count: data.len() as u32,
        image_tool_usage_reported: upstream.is_object(),
        source: "images_response".into(),
        complete: input.is_some()
            && output.is_some()
            && image_input.is_some()
            && image_output.is_some(),
        raw_usage: raw,
        ..Default::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn codex_images_are_stateless_and_edits_preserve_bytes() {
        let headers =
            HeaderMap::from_iter([(TURN_HEADER.parse().unwrap(), "turn-1".parse().unwrap())]);
        let body = json!({"model":"gpt-image-2","prompt":"PRIVATE_PROMPT","images":[{"image_url":"data:image/png;base64,PRIVATE_IMAGE"}],"quality":"low"});
        let r = parse(&headers, body.clone(), RequestKind::ImageEdit).unwrap();
        assert!(r.stateless);
        assert!(!r.stream);
        assert_eq!(r.document.value, body);
        assert_eq!(r.identity_headers[TURN_HEADER], "turn-1");
        for body in [
            json!({}),
            json!({"model":"gpt-image-2","prompt":"hi","stream":true}),
            json!({"model":"gpt-image-2","prompt":"hi","images":[]}),
        ] {
            assert!(parse(&HeaderMap::new(), body, RequestKind::ImageEdit).is_err());
        }
    }
    #[test]
    fn only_numeric_usage_survives_and_missing_usage_is_unknown() {
        let body = json!({"data":[{"b64_json":"PRIVATE_IMAGE","revised_prompt":"PRIVATE_PROMPT"}],"safety_identifier":"PRIVATE_ID","usage":{"input_tokens":20,"output_tokens":515,"input_tokens_details":{"text_tokens":18,"image_tokens":2},"output_tokens_details":{"text_tokens":0,"image_tokens":515},"unknown":"PRIVATE_USAGE"}});
        let usage = response(&serde_json::to_vec(&body).unwrap()).unwrap();
        assert!(usage.complete);
        assert_eq!(usage.input_tokens, Some(20));
        assert_eq!(usage.image_output_tokens, Some(515));
        assert!(!serde_json::to_string(&usage).unwrap().contains("PRIVATE_"));
        let missing = response(br#"{"data":[{"b64_json":"image"}]}"#).unwrap();
        assert!(!missing.complete);
        assert_eq!(missing.input_tokens, None);
        for bad in [br#"{}"#.as_slice(), br#"{"data":[]}"#, br#"{"data":[{}]}"#] {
            assert!(response(bad).is_err());
        }
    }
}
