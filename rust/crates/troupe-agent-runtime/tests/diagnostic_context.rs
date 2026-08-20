use std::{fs, path::PathBuf};

use agent_client_protocol::schema::v1::{Cost, SessionUpdate, UsageUpdate};
use serde_json::json;
use troupe_agent_runtime::{
    AgentDiagnosticCandidate,
    diagnostics::context::{
        AGENT_CONTEXT_OCCUPANCY_CANDIDATE_KIND, AGENT_CONTEXT_USAGE_SAMPLE_CANDIDATE_KIND,
        AgentContextOccupancy, AgentContextOccupancyCandidate, AgentContextOccupancyError,
        AgentContextUsageSampleCandidate,
    },
};

#[test]
fn used_and_window_are_independently_optional_without_estimation() {
    for (used, window) in [
        (None, None),
        (Some(0), None),
        (None, Some(0)),
        (Some(7), Some(11)),
    ] {
        let occupancy = AgentContextOccupancy::new(used, window).unwrap();
        assert_eq!(occupancy.context_used_tokens(), used);
        assert_eq!(occupancy.context_window_tokens(), window);
    }
}

#[test]
fn present_pair_accepts_zero_equal_and_full_u64_range() {
    for (used, window) in [(0, 0), (7, 7), (u64::MAX - 1, u64::MAX)] {
        let occupancy = AgentContextOccupancy::new(Some(used), Some(window)).unwrap();
        assert_eq!(occupancy.context_used_tokens(), Some(used));
        assert_eq!(occupancy.context_window_tokens(), Some(window));
    }
}

#[test]
fn used_above_window_is_a_stable_rejection() {
    let error = AgentContextOccupancy::new(Some(12), Some(11)).unwrap_err();
    assert_eq!(error, AgentContextOccupancyError::UsedExceedsWindow);
    assert_eq!(error.code(), "used_exceeds_context_window");
    assert_eq!(error.to_string(), "used_exceeds_context_window");

    let SessionUpdate::UsageUpdate(update) = serde_json::from_value(json!({
        "sessionUpdate": "usage_update",
        "used": 12,
        "size": 11
    }))
    .unwrap() else {
        panic!("expected ACP usage update");
    };
    assert_eq!(
        AgentContextOccupancy::from_acp(&update),
        Err(AgentContextOccupancyError::UsedExceedsWindow)
    );
}

#[test]
fn observations_can_rise_or_fall_and_keep_their_original_elapsed_time() {
    let observations = [(800, 1), (125, 2), (900, 3)].map(|(used, elapsed)| {
        AgentContextOccupancy::new(Some(used), Some(1_000))
            .unwrap()
            .observed_at(elapsed)
    });

    assert_eq!(observations[0].context_used_tokens(), Some(800));
    assert_eq!(observations[1].context_used_tokens(), Some(125));
    assert_eq!(observations[2].context_used_tokens(), Some(900));
    assert_eq!(observations[0].observed_elapsed_ns(), 1);
    assert_eq!(observations[1].observed_elapsed_ns(), 2);
    assert_eq!(observations[2].observed_elapsed_ns(), 3);
    assert_eq!(
        observations[1].occupancy().context_window_tokens(),
        Some(1_000)
    );
}

#[test]
fn acp_mapping_ignores_cost_and_protocol_metadata() {
    let update = UsageUpdate::new(53_000, 200_000)
        .cost(Cost::new(0.045, "USD"))
        .meta(
            json!({"provider_secret": "must-not-survive"})
                .as_object()
                .cloned(),
        );
    let occupancy = AgentContextOccupancy::from_acp(&update).unwrap();

    assert_eq!(occupancy.context_used_tokens(), Some(53_000));
    assert_eq!(occupancy.context_window_tokens(), Some(200_000));
    let debug = format!("{occupancy:?}");
    assert!(!debug.contains("USD"));
    assert!(!debug.contains("provider_secret"));
}

#[test]
fn adapter_rejects_bool_negative_fraction_overflow_and_missing_required_carrier_fields() {
    let malformed = [
        json!({"sessionUpdate": "usage_update", "used": true, "size": 10}),
        json!({"sessionUpdate": "usage_update", "used": 1, "size": false}),
        json!({"sessionUpdate": "usage_update", "used": -1, "size": 10}),
        json!({"sessionUpdate": "usage_update", "used": 1.5, "size": 10}),
        json!({
            "sessionUpdate": "usage_update",
            "used": 18_446_744_073_709_551_616_u128,
            "size": 18_446_744_073_709_551_616_u128
        }),
        json!({"sessionUpdate": "usage_update", "size": 10}),
        json!({"sessionUpdate": "usage_update", "used": 1}),
    ];
    for value in malformed {
        assert!(
            serde_json::from_value::<SessionUpdate>(value.clone()).is_err(),
            "adapter accepted malformed usage value: {value}"
        );
    }

    let SessionUpdate::UsageUpdate(update) = serde_json::from_value(json!({
        "sessionUpdate": "usage_update",
        "used": u64::MAX,
        "size": u64::MAX
    }))
    .unwrap() else {
        panic!("expected ACP usage update");
    };
    assert_eq!(update.used, u64::MAX);
    assert_eq!(update.size, u64::MAX);
}

#[test]
fn candidate_types_are_pre_sequence_and_b12_stamps_the_observation_time() {
    fn assert_candidate<T: AgentDiagnosticCandidate>() {}
    assert_candidate::<AgentContextOccupancyCandidate>();
    assert_candidate::<AgentContextUsageSampleCandidate>();
    assert_eq!(AGENT_CONTEXT_OCCUPANCY_CANDIDATE_KIND, "context_occupancy");
    assert_eq!(
        AGENT_CONTEXT_USAGE_SAMPLE_CANDIDATE_KIND,
        "context_usage_sampled"
    );

    let crate_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let source = fs::read_to_string(crate_root.join("src/diagnostics/context.rs")).unwrap();
    for required in [
        "SessionUpdate::UsageUpdate",
        "AgentContextOccupancy::from_acp",
        "AgentDiagnosticObservation::Candidate",
        "observed_elapsed_ns",
    ] {
        assert!(
            source.contains(required),
            "missing context seam: {required}"
        );
    }
    for forbidden in [
        "ActTokenUsageFinalized",
        "consumed_tokens",
        "previous_used_tokens",
        "token_delta",
        "checked_sub",
        "saturating_sub",
        "sequence",
    ] {
        assert!(
            !source.contains(forbidden),
            "context occupancy must not contain {forbidden}"
        );
    }
}
