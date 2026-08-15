use std::{fs, path::PathBuf, sync::Arc};

use agent_client_protocol::schema::v1::{Cost, Meta};
use serde_json::json;
use troupe_agent_runtime::{
    AgentDiagnosticCandidate, AgentDiagnosticProvider, AgentSessionDiagnosticContext,
    diagnostics::cost::{
        AGENT_COST_CANDIDATE_KIND, AgentCostCandidate, AgentCostDetail,
        AgentCostNormalizationError, AgentCumulativeCost,
    },
};

fn normalized(
    amount: Option<&str>,
    currency: Option<&str>,
) -> Result<Option<AgentCumulativeCost>, AgentCostNormalizationError> {
    AgentCumulativeCost::from_decimal_pair(amount, currency)
}

#[test]
fn exact_decimal_amount_is_canonical_without_binary_float_storage() {
    let cases = [
        ("0", "0"),
        ("-0.000", "0"),
        ("+00012.3400", "12.34"),
        ("1e-7", "0.0000001"),
        ("0.1000000000000000000001", "0.1000000000000000000001"),
    ];

    for (input, expected) in cases {
        let cost = normalized(Some(input), Some("USD")).unwrap().unwrap();
        assert_eq!(cost.amount().as_str(), expected);
        assert_eq!(cost.currency().as_str(), "USD");
    }
}

#[test]
fn absence_is_distinct_from_zero_and_half_pairs_are_rejected() {
    assert_eq!(normalized(None, None), Ok(None));

    let zero = normalized(Some("0"), Some("USD")).unwrap().unwrap();
    assert_eq!(zero.amount().as_str(), "0");
    assert_eq!(zero.currency().as_str(), "USD");

    assert_eq!(
        normalized(Some("1"), None),
        Err(AgentCostNormalizationError::AmountWithoutCurrency)
    );
    assert_eq!(
        normalized(None, Some("USD")),
        Err(AgentCostNormalizationError::CurrencyWithoutAmount)
    );
}

#[test]
fn negative_nonfinite_and_invalid_currency_values_are_rejected() {
    for amount in ["-0.01", "-1e2"] {
        assert_eq!(
            normalized(Some(amount), Some("USD")),
            Err(AgentCostNormalizationError::NegativeAmount),
            "{amount}"
        );
    }
    for amount in ["NaN", "Infinity", "-Infinity"] {
        assert_eq!(
            normalized(Some(amount), Some("USD")),
            Err(AgentCostNormalizationError::NonFiniteAmount),
            "{amount}"
        );
    }
    for amount in ["", ".", "1.2.3", "money"] {
        assert_eq!(
            normalized(Some(amount), Some("USD")),
            Err(AgentCostNormalizationError::InvalidAmount),
            "{amount}"
        );
    }
    for currency in ["", "US", "USDD", "usd", "U1D", "EURO"] {
        assert_eq!(
            normalized(Some("1"), Some(currency)),
            Err(AgentCostNormalizationError::InvalidCurrency),
            "{currency}"
        );
    }
}

#[test]
fn acp_float_boundary_rejects_nonfinite_and_negative_but_preserves_zero() {
    assert_eq!(AgentCumulativeCost::from_acp(None), Ok(None));

    for amount in [0.0, -0.0] {
        let source = Cost::new(amount, "EUR");
        let cost = AgentCumulativeCost::from_acp(Some(&source))
            .unwrap()
            .unwrap();
        assert_eq!(cost.amount().as_str(), "0");
        assert_eq!(cost.currency().as_str(), "EUR");
    }

    let exact = AgentCumulativeCost::from_acp(Some(&Cost::new(0.125, "USD")))
        .unwrap()
        .unwrap();
    assert_eq!(exact.amount().as_str(), "0.125");

    for amount in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        assert_eq!(
            AgentCumulativeCost::from_acp(Some(&Cost::new(amount, "USD"))),
            Err(AgentCostNormalizationError::NonFiniteAmount)
        );
    }
    assert_eq!(
        AgentCumulativeCost::from_acp(Some(&Cost::new(-0.01, "USD"))),
        Err(AgentCostNormalizationError::NegativeAmount)
    );
}

#[test]
fn provider_and_model_are_bounded_typed_detail_not_acp_metadata() {
    let detail = AgentCostDetail::new(
        AgentSessionDiagnosticContext::new("actor-1", "session-1"),
        Some(7),
        AgentDiagnosticProvider::Claude,
        Some(Arc::from("claude-model")),
    );
    assert_eq!(detail.session().actor_id(), "actor-1");
    assert_eq!(detail.session().session_id(), "session-1");
    assert_eq!(detail.session_generation(), Some(7));
    assert_eq!(detail.provider(), AgentDiagnosticProvider::Claude);
    assert_eq!(detail.effective_model(), Some("claude-model"));

    let mut meta = Meta::new();
    meta.insert("provider".to_owned(), json!("spoofed-provider"));
    meta.insert("model".to_owned(), json!("spoofed-model"));
    meta.insert("rawEnvelope".to_owned(), json!({"secret": true}));
    let source = Cost::new(1.25, "USD").meta(meta);
    let cost = AgentCumulativeCost::from_acp(Some(&source))
        .unwrap()
        .unwrap();
    let candidate = AgentCostCandidate::new(detail.clone(), cost.clone());

    assert_eq!(candidate.detail(), &detail);
    assert_eq!(candidate.cost(), &cost);
    assert_eq!(candidate.kind(), AGENT_COST_CANDIDATE_KIND);
    assert_eq!(AGENT_COST_CANDIDATE_KIND, "agent_cumulative_cost");
}

#[test]
fn candidate_contract_is_session_scoped_cost_not_act_token_usage_or_billing_state() {
    fn assert_candidate<T: AgentDiagnosticCandidate>() {}
    assert_candidate::<AgentCostCandidate>();

    let crate_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let source = fs::read_to_string(crate_root.join("src/diagnostics/cost.rs"))
        .expect("read cost normalizer source");
    for required in [
        "SessionUpdate::UsageUpdate",
        "AgentDiagnosticObservation::Candidate",
        "AgentCostDetail",
        "DecimalString",
        "CurrencyCode",
    ] {
        assert!(
            source.contains(required),
            "cost contract is missing {required}"
        );
    }
    for forbidden in [
        "TokenCount",
        "ActTokenUsageFinalized",
        "input_tokens",
        "output_tokens",
        "act_id",
        "HashMap",
        "BTreeMap",
        "conversion_rate",
        "display_currency",
        "cost.meta",
        "usage.meta",
    ] {
        assert!(
            !source.contains(forbidden),
            "cost normalization must not retain or derive {forbidden}"
        );
    }
}
