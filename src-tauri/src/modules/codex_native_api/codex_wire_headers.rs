//! Derive backend routing hints after all body/model/tier mutations (CPA v8).
use super::ServiceError;
use axum::http::{HeaderMap, HeaderValue};
use serde_json::Value;

const ROUTING_HINT: &str = "x-codex-routing-hint";
const ACCOUNT_ROUTING_OVERRIDE: &str = "x-openai-account-routing-override";
const ACCOUNT_ROUTING_OVERRIDE_ENV: &str = "COCKPIT_CODEX_ACCOUNT_ROUTING_OVERRIDE";
const SUPPORTED_ACCOUNT_ROUTING_OVERRIDE: &str = "us_cr";

pub(crate) fn finalize_routing_hint(
    headers: &mut HeaderMap,
    body: &Value,
    chatgpt_backend: bool,
    explicit_headers: &HeaderMap,
) -> Result<(), ServiceError> {
    if !chatgpt_backend {
        return Ok(());
    }
    headers.remove(ROUTING_HINT);
    // Selected account/model header overrides are operator-owned. A forwarded
    // client hint is not: it may still name the pre-mapping parent model/tier.
    if let Some(value) = explicit_headers
        .get(ROUTING_HINT)
        .filter(|value| value.to_str().is_ok_and(|value| !value.trim().is_empty()))
    {
        headers.insert(ROUTING_HINT, value.clone());
        return Ok(());
    }
    let Some(model) = body
        .get("model")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return Ok(());
    };
    let mut hint = format!("model={model}");
    if let Some(tier) = body
        .get("service_tier")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|tier| !tier.is_empty())
    {
        hint.push_str(";tier=");
        hint.push_str(tier);
    }
    let value = HeaderValue::from_str(&hint).map_err(|_| {
        ServiceError::new(
            400,
            "invalid_protocol_identity",
            "Model or service tier cannot form a routing header",
        )
    })?;
    headers.insert(ROUTING_HINT, value);
    Ok(())
}

/// Apply the account routing override only to the official Codex backend. The
/// downstream caller cannot supply this internal header. `us_cr` is the
/// default; setting `COCKPIT_CODEX_ACCOUNT_ROUTING_OVERRIDE` disables it.
pub(crate) fn apply_account_routing_override(
    headers: &mut HeaderMap,
    chatgpt_backend: bool,
    disabled: bool,
) {
    headers.remove(ACCOUNT_ROUTING_OVERRIDE);
    if !chatgpt_backend || disabled {
        return;
    }
    headers.insert(
        ACCOUNT_ROUTING_OVERRIDE,
        HeaderValue::from_static(SUPPORTED_ACCOUNT_ROUTING_OVERRIDE),
    );
}

pub(crate) fn account_routing_override_disabled() -> bool {
    std::env::var_os(ACCOUNT_ROUTING_OVERRIDE_ENV).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn backend_routing_hint_uses_final_model_and_normalized_service_tier() {
        let mut headers = HeaderMap::from_iter([(
            ROUTING_HINT.parse().unwrap(),
            HeaderValue::from_static("model=parent;tier=fast"),
        )]);
        finalize_routing_hint(
            &mut headers,
            &json!({"model":"child-mapped","service_tier":"priority"}),
            true,
            &HeaderMap::new(),
        )
        .unwrap();
        assert_eq!(headers[ROUTING_HINT], "model=child-mapped;tier=priority");
        finalize_routing_hint(
            &mut headers,
            &json!({"model":"child-mapped"}),
            true,
            &HeaderMap::new(),
        )
        .unwrap();
        assert_eq!(headers[ROUTING_HINT], "model=child-mapped");
    }

    #[test]
    fn backend_routing_hint_preserves_explicit_selected_account_override() {
        let mut headers = HeaderMap::new();
        let mut overrides = HeaderMap::from_iter([(
            ROUTING_HINT.parse().unwrap(),
            HeaderValue::from_static("operator-route"),
        )]);
        finalize_routing_hint(&mut headers, &json!({"model":"child"}), true, &overrides).unwrap();
        assert_eq!(headers[ROUTING_HINT], "operator-route");
        overrides.insert(ROUTING_HINT, HeaderValue::from_static("  "));
        finalize_routing_hint(&mut headers, &json!({"model":"child"}), true, &overrides).unwrap();
        assert_eq!(headers[ROUTING_HINT], "model=child");
    }

    #[test]
    fn api_key_and_siwc_routing_headers_are_outside_backend_policy() {
        let mut headers = HeaderMap::from_iter([(
            ROUTING_HINT.parse().unwrap(),
            HeaderValue::from_static("explicit-original"),
        )]);
        finalize_routing_hint(
            &mut headers,
            &json!({"model":"child","service_tier":"priority"}),
            false,
            &HeaderMap::new(),
        )
        .unwrap();
        assert_eq!(headers[ROUTING_HINT], "explicit-original");
        let mut empty = HeaderMap::new();
        finalize_routing_hint(
            &mut empty,
            &json!({"model":"child"}),
            false,
            &HeaderMap::new(),
        )
        .unwrap();
        assert!(!empty.contains_key(ROUTING_HINT));
    }

    #[test]
    fn backend_routing_hint_rejects_control_characters_and_clears_missing_model() {
        let mut headers = HeaderMap::new();
        assert!(finalize_routing_hint(
            &mut headers,
            &json!({"model":"child\nforged: value"}),
            true,
            &HeaderMap::new()
        )
        .is_err());
        headers.insert(ROUTING_HINT, HeaderValue::from_static("stale"));
        finalize_routing_hint(&mut headers, &json!({"model":" "}), true, &HeaderMap::new())
            .unwrap();
        assert!(!headers.contains_key(ROUTING_HINT));
    }

    #[test]
    fn account_routing_override_defaults_to_us_cr_and_can_be_disabled() {
        let mut headers = HeaderMap::from_iter([(
            ACCOUNT_ROUTING_OVERRIDE.parse().unwrap(),
            HeaderValue::from_static("caller-controlled"),
        )]);
        apply_account_routing_override(&mut headers, false, false);
        assert!(!headers.contains_key(ACCOUNT_ROUTING_OVERRIDE));

        headers.insert(
            ACCOUNT_ROUTING_OVERRIDE,
            HeaderValue::from_static("caller-controlled"),
        );
        apply_account_routing_override(&mut headers, true, false);
        assert_eq!(
            headers[ACCOUNT_ROUTING_OVERRIDE],
            SUPPORTED_ACCOUNT_ROUTING_OVERRIDE
        );
        apply_account_routing_override(&mut headers, true, true);
        assert!(!headers.contains_key(ACCOUNT_ROUTING_OVERRIDE));
    }

    #[test]
    fn native_instruction_text_is_preserved_without_whitespace_replacement() {
        for instructions in ["", "  \n\t", "  inherited agent instructions\n"] {
            let mut body = json!({"input":[],"instructions":instructions});
            super::super::normalize(&mut HeaderMap::new(), &mut body, true, false).unwrap();
            assert_eq!(body["instructions"], instructions);
        }
    }
}
