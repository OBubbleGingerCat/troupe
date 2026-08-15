use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use agent_client_protocol::schema::v1::{PromptResponse, StopReason, Usage};
use serde_json::Value;
use troupe_agent_runtime::{
    AgentDiagnosticCandidate, AgentDiagnosticProvider,
    diagnostics::usage::{
        ACP_TURN_USAGE_CLIENT_SDK_VERSION, ACP_TURN_USAGE_SOURCE, AGENT_TURN_USAGE_CANDIDATE_KIND,
        AgentTurnUsage, AgentTurnUsageCandidate, AgentTurnUsageValidationError,
        AgentUsageQualification, QUALIFIED_CLAUDE_ADAPTER_PACKAGE,
        QUALIFIED_CLAUDE_ADAPTER_VERSION, QUALIFIED_CODEX_ADAPTER_PACKAGE,
        QUALIFIED_CODEX_ADAPTER_VERSION, TokenCount, UsageAvailability, UsageSource,
        UsageUnavailableReason, pinned_usage_qualification,
    },
};

const QUALIFICATION_FIXTURE: &str =
    include_str!("../../../../tests/fixtures/acp/usage/qualification.json");
const CODEX_AVAILABLE_FIXTURE: &str =
    include_str!("../../../../tests/fixtures/acp/usage/codex-available.json");
const CLAUDE_PARTIAL_FIXTURE: &str =
    include_str!("../../../../tests/fixtures/acp/usage/claude-partial.json");
const KIMI_UNAVAILABLE_FIXTURE: &str =
    include_str!("../../../../tests/fixtures/acp/usage/kimi-unavailable.json");
const MALFORMED_FIXTURE: &str = include_str!("../../../../tests/fixtures/acp/usage/malformed.json");

fn fixture(source: &str) -> Value {
    serde_json::from_str(source).expect("valid frozen usage fixture")
}

fn crate_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn repository_root() -> PathBuf {
    crate_root().join("../../..")
}

fn source(path: impl AsRef<Path>) -> String {
    fs::read_to_string(path).expect("read source file")
}

fn provider(name: &str) -> AgentDiagnosticProvider {
    match name {
        "codex" => AgentDiagnosticProvider::Codex,
        "claude" => AgentDiagnosticProvider::Claude,
        "kimi" => AgentDiagnosticProvider::Kimi,
        _ => panic!("unknown provider fixture: {name}"),
    }
}

fn prompt_response(turn: &Value) -> PromptResponse {
    serde_json::from_value(turn["promptResponse"].clone())
        .expect("fixture contains an ACP PromptResponse")
}

fn token(value: &str) -> TokenCount {
    TokenCount::parse(value).expect("canonical token count")
}

fn usage_from_fields(
    availability: UsageAvailability,
    source: Option<UsageSource>,
    unavailable_reason: Option<UsageUnavailableReason>,
    fields: [Option<TokenCount>; 6],
) -> Result<AgentTurnUsage, AgentTurnUsageValidationError> {
    let [total, input, output, thought, cached_read, cached_write] = fields;
    AgentTurnUsage::new(
        availability,
        source,
        unavailable_reason,
        total,
        input,
        output,
        thought,
        cached_read,
        cached_write,
    )
}

fn no_fields() -> [Option<TokenCount>; 6] {
    std::array::from_fn(|_| None)
}

fn complete_fields() -> [Option<TokenCount>; 6] {
    [
        Some(token("9")),
        Some(token("4")),
        Some(token("5")),
        None,
        None,
        None,
    ]
}

fn assert_no_token_values(usage: &AgentTurnUsage) {
    assert!(usage.provider_total_tokens().is_none());
    assert!(usage.input_tokens().is_none());
    assert!(usage.output_tokens().is_none());
    assert!(usage.thought_tokens().is_none());
    assert!(usage.cached_read_tokens().is_none());
    assert!(usage.cached_write_tokens().is_none());
}

fn assert_acp_fields(usage: &AgentTurnUsage, acp: &Usage) {
    let total = acp.total_tokens.to_string();
    let input = acp.input_tokens.to_string();
    let output = acp.output_tokens.to_string();
    let thought = acp.thought_tokens.map(|value| value.to_string());
    let cached_read = acp.cached_read_tokens.map(|value| value.to_string());
    let cached_write = acp.cached_write_tokens.map(|value| value.to_string());

    assert_eq!(
        usage.provider_total_tokens().map(TokenCount::as_str),
        Some(total.as_str())
    );
    assert_eq!(
        usage.input_tokens().map(TokenCount::as_str),
        Some(input.as_str())
    );
    assert_eq!(
        usage.output_tokens().map(TokenCount::as_str),
        Some(output.as_str())
    );
    assert_eq!(
        usage.thought_tokens().map(TokenCount::as_str),
        thought.as_deref()
    );
    assert_eq!(
        usage.cached_read_tokens().map(TokenCount::as_str),
        cached_read.as_deref()
    );
    assert_eq!(
        usage.cached_write_tokens().map(TokenCount::as_str),
        cached_write.as_deref()
    );
}

fn assert_primary_evidence_is_whole_turn(turn: &Value, response: &PromptResponse) {
    let requests = turn["modelRequestUsage"]
        .as_array()
        .expect("qualification evidence is a request list");
    assert!(!requests.is_empty());
    let usage = response
        .usage
        .as_ref()
        .expect("qualified fixture reports terminal usage");
    for (field, terminal) in [
        ("totalTokens", usage.total_tokens),
        ("inputTokens", usage.input_tokens),
        ("outputTokens", usage.output_tokens),
    ] {
        let request_sum = requests
            .iter()
            .map(|request| {
                request[field]
                    .as_u64()
                    .expect("request evidence is an ACP u64")
            })
            .sum::<u64>();
        assert_eq!(terminal, request_sum, "{field} is not whole-turn usage");
    }
}

fn assert_qualified_fixture(
    source: &str,
    expected_provider: AgentDiagnosticProvider,
    expected_profile: &str,
    expected_shapes: &[&str],
) -> Vec<AgentTurnUsage> {
    let fixture = fixture(source);
    assert_eq!(fixture["adapterProfile"], expected_profile);
    let turns = fixture["turns"].as_array().expect("fixture turns");
    assert_eq!(
        turns
            .iter()
            .map(|turn| turn["shape"].as_str().expect("turn shape"))
            .collect::<Vec<_>>(),
        expected_shapes
    );

    turns
        .iter()
        .map(|turn| {
            let response = prompt_response(turn);
            assert_primary_evidence_is_whole_turn(turn, &response);
            let normalized =
                AgentTurnUsage::from_prompt_response(expected_provider, Some(&response));
            assert_eq!(normalized.availability(), UsageAvailability::Available);
            assert_eq!(
                normalized.source(),
                Some(UsageSource::AcpPromptResponseUsage)
            );
            assert_eq!(normalized.unavailable_reason(), None);
            assert_acp_fields(&normalized, response.usage.as_ref().unwrap());
            normalized
        })
        .collect()
}

fn launch_profile_block<'a>(launch: &'a str, provider: &str) -> &'a str {
    let marker = format!("const {}: AgentLaunchSpec", provider.to_ascii_uppercase());
    let tail = &launch[launch.find(&marker).expect("pinned launch profile")..];
    &tail[..tail.find("\n};").expect("launch profile end") + 3]
}

#[test]
fn qualification_fixture_pins_only_proven_whole_turn_adapters() {
    let fixture = fixture(QUALIFICATION_FIXTURE);
    assert_eq!(fixture["schemaVersion"], 1);
    assert_eq!(fixture["carrier"], ACP_TURN_USAGE_SOURCE);
    assert_eq!(fixture["acp"]["version"], ACP_TURN_USAGE_CLIENT_SDK_VERSION);
    assert_eq!(
        fixture["acp"]["features"],
        serde_json::json!(["unstable_end_turn_token_usage"])
    );

    let launch = source(crate_root().join("src/launch/mod.rs"));
    let adapters = fixture["adapters"].as_array().expect("adapter profiles");
    assert_eq!(adapters.len(), 3);
    for adapter in adapters {
        let name = adapter["provider"].as_str().expect("provider name");
        let qualification = pinned_usage_qualification(provider(name));
        let expected = if adapter["qualified"].as_bool().unwrap() {
            AgentUsageQualification::WholeTurn
        } else {
            AgentUsageQualification::SourceUnsupported
        };
        assert_eq!(qualification, expected, "{name}");

        let block = launch_profile_block(&launch, name);
        let package = adapter["package"].as_str().expect("adapter package");
        let version = adapter["version"].as_str().expect("adapter version");
        let runner_field = if adapter["runner"] == "npx" {
            "package"
        } else {
            "program"
        };
        assert!(
            block.contains(&format!("{runner_field}: \"{package}\"")),
            "{name} package drifted"
        );
        assert!(
            block.contains(&format!("exact_version: \"{version}\"")),
            "{name} version drifted"
        );

        let evidence = adapter["evidence"]
            .as_array()
            .unwrap()
            .iter()
            .map(|value| value.as_str().unwrap())
            .collect::<BTreeSet<_>>();
        match name {
            "codex" => assert_eq!(evidence, BTreeSet::from(["single_request", "tool_loop"])),
            "claude" => {
                assert_eq!(
                    evidence,
                    BTreeSet::from(["multi_request", "single_request"])
                )
            }
            "kimi" => assert!(evidence.is_empty()),
            _ => unreachable!(),
        }
    }

    assert_eq!(
        QUALIFIED_CODEX_ADAPTER_PACKAGE,
        "@agentclientprotocol/codex-acp"
    );
    assert_eq!(QUALIFIED_CODEX_ADAPTER_VERSION, "1.1.9");
    assert_eq!(
        QUALIFIED_CLAUDE_ADAPTER_PACKAGE,
        "@agentclientprotocol/claude-agent-acp"
    );
    assert_eq!(QUALIFIED_CLAUDE_ADAPTER_VERSION, "0.64.2");
}

#[test]
fn manifest_enables_only_the_single_required_acp_feature() {
    let manifest = source(crate_root().join("Cargo.toml"));
    let dependency = manifest
        .lines()
        .filter(|line| line.trim_start().starts_with("agent-client-protocol ="))
        .collect::<Vec<_>>();
    assert_eq!(
        dependency,
        [
            "agent-client-protocol = { version = \"=2.0.0\", features = [\"unstable_end_turn_token_usage\"] }"
        ]
    );
    assert_eq!(manifest.matches("unstable_end_turn_token_usage").count(), 1);
}

#[test]
fn codex_and_claude_terminal_fixtures_are_qualified_only_after_whole_turn_settlement() {
    let codex = assert_qualified_fixture(
        CODEX_AVAILABLE_FIXTURE,
        AgentDiagnosticProvider::Codex,
        "codex-acp@1.1.9",
        &["single_request", "tool_loop"],
    );
    assert_eq!(
        codex[1].provider_total_tokens().map(TokenCount::as_str),
        Some("35")
    );
    assert_eq!(
        codex[1].cached_write_tokens().map(TokenCount::as_str),
        Some("0")
    );

    let claude = assert_qualified_fixture(
        CLAUDE_PARTIAL_FIXTURE,
        AgentDiagnosticProvider::Claude,
        "claude-agent-acp@0.64.2",
        &["single_request", "multi_request"],
    );
    assert_eq!(
        claude[0].provider_total_tokens().map(TokenCount::as_str),
        Some("0")
    );
    assert_eq!(claude[0].input_tokens().map(TokenCount::as_str), Some("0"));
    assert_eq!(claude[0].output_tokens().map(TokenCount::as_str), Some("0"));
    assert!(claude[0].thought_tokens().is_none());
    assert!(claude[1].cached_read_tokens().is_none());
    assert_eq!(claude[1].availability(), UsageAvailability::Available);
}

#[test]
fn kimi_stays_source_unsupported_even_if_the_terminal_response_has_numbers() {
    let fixture = fixture(KIMI_UNAVAILABLE_FIXTURE);
    assert_eq!(fixture["adapterProfile"], "kimi-code@0.31.1");
    assert_eq!(
        pinned_usage_qualification(AgentDiagnosticProvider::Kimi),
        AgentUsageQualification::SourceUnsupported
    );

    for turn in fixture["turns"].as_array().unwrap() {
        let response = prompt_response(turn);
        let normalized =
            AgentTurnUsage::from_prompt_response(AgentDiagnosticProvider::Kimi, Some(&response));
        assert_eq!(normalized.availability(), UsageAvailability::Unavailable);
        assert_eq!(normalized.source(), None);
        assert_eq!(
            normalized.unavailable_reason(),
            Some(UsageUnavailableReason::SourceUnsupported)
        );
        assert_no_token_values(&normalized);
        let debug = format!("{normalized:?}");
        assert!(!debug.contains("must-not-survive"));
        assert!(!debug.contains("/private/session.log"));
    }
}

#[test]
fn malformed_or_absent_typed_carriers_never_become_partial_raw_data() {
    let fixture = fixture(MALFORMED_FIXTURE);
    assert_eq!(fixture["qualifiedProvider"], "codex");
    for case in fixture["cases"].as_array().unwrap() {
        let response = prompt_response(case);
        let normalized =
            AgentTurnUsage::from_prompt_response(AgentDiagnosticProvider::Codex, Some(&response));
        assert_eq!(
            normalized.availability().as_str(),
            case["expectedAvailability"].as_str().unwrap(),
            "{}",
            case["name"]
        );
        assert_eq!(
            normalized
                .unavailable_reason()
                .map(UsageUnavailableReason::as_str),
            case["expectedReason"].as_str(),
            "{}",
            case["name"]
        );
        if normalized.availability() == UsageAvailability::Unavailable {
            assert_eq!(normalized.source(), None);
            assert_no_token_values(&normalized);
        } else {
            assert_eq!(
                normalized.source(),
                Some(UsageSource::AcpPromptResponseUsage)
            );
            assert_eq!(
                normalized.cached_write_tokens().map(TokenCount::as_str),
                Some("0")
            );
            assert!(normalized.thought_tokens().is_none());
            assert!(normalized.cached_read_tokens().is_none());
        }
    }
}

#[test]
fn available_partial_and_unavailable_combinations_are_closed_and_exact() {
    let beyond_u64 = token("184467440737095516160000000000000000000000000001");
    let available = usage_from_fields(
        UsageAvailability::Available,
        Some(UsageSource::AcpPromptResponseUsage),
        None,
        [
            Some(beyond_u64),
            Some(token("0")),
            Some(token("2")),
            None,
            Some(token("0")),
            None,
        ],
    )
    .unwrap();
    assert_eq!(available.availability(), UsageAvailability::Available);
    assert_eq!(
        available.provider_total_tokens().map(TokenCount::as_str),
        Some("184467440737095516160000000000000000000000000001")
    );
    assert_eq!(available.input_tokens().map(TokenCount::as_str), Some("0"));
    assert!(available.thought_tokens().is_none());

    for present_field in 0..6 {
        let mut fields = no_fields();
        fields[present_field] = Some(token(if present_field == 4 { "0" } else { "7" }));
        let partial = usage_from_fields(
            UsageAvailability::Partial,
            Some(UsageSource::AcpPromptResponseUsage),
            None,
            fields,
        )
        .unwrap();
        assert_eq!(partial.availability(), UsageAvailability::Partial);
        assert_eq!(partial.source(), Some(UsageSource::AcpPromptResponseUsage));
        assert_eq!(partial.unavailable_reason(), None);
    }

    for reason in [
        UsageUnavailableReason::PromptNotSubmitted,
        UsageUnavailableReason::SourceUnsupported,
        UsageUnavailableReason::UsageNotReported,
        UsageUnavailableReason::TurnSettlementUnknown,
    ] {
        let unavailable = AgentTurnUsage::unavailable(reason);
        assert_eq!(unavailable.availability(), UsageAvailability::Unavailable);
        assert_eq!(unavailable.source(), None);
        assert_eq!(unavailable.unavailable_reason(), Some(reason));
        assert_no_token_values(&unavailable);
    }

    let invalid = [
        usage_from_fields(UsageAvailability::Available, None, None, complete_fields()),
        usage_from_fields(
            UsageAvailability::Available,
            Some(UsageSource::AcpPromptResponseUsage),
            Some(UsageUnavailableReason::UsageNotReported),
            complete_fields(),
        ),
        usage_from_fields(
            UsageAvailability::Available,
            Some(UsageSource::AcpPromptResponseUsage),
            None,
            [Some(token("1")), Some(token("1")), None, None, None, None],
        ),
        usage_from_fields(
            UsageAvailability::Partial,
            Some(UsageSource::AcpPromptResponseUsage),
            None,
            complete_fields(),
        ),
        usage_from_fields(
            UsageAvailability::Partial,
            Some(UsageSource::AcpPromptResponseUsage),
            None,
            no_fields(),
        ),
        usage_from_fields(
            UsageAvailability::Partial,
            None,
            None,
            [Some(token("1")), None, None, None, None, None],
        ),
        usage_from_fields(
            UsageAvailability::Partial,
            Some(UsageSource::AcpPromptResponseUsage),
            Some(UsageUnavailableReason::SourceUnsupported),
            [Some(token("1")), None, None, None, None, None],
        ),
        usage_from_fields(UsageAvailability::Unavailable, None, None, no_fields()),
        usage_from_fields(
            UsageAvailability::Unavailable,
            Some(UsageSource::AcpPromptResponseUsage),
            Some(UsageUnavailableReason::UsageNotReported),
            no_fields(),
        ),
        usage_from_fields(
            UsageAvailability::Unavailable,
            None,
            Some(UsageUnavailableReason::UsageNotReported),
            [None, None, None, Some(token("1")), None, None],
        ),
    ];
    for result in invalid {
        let error = result.unwrap_err();
        assert_eq!(error.code(), "inconsistent_terminal_usage");
        assert_eq!(error.to_string(), "inconsistent_terminal_usage");
    }
}

#[test]
fn provider_total_is_preserved_and_never_synthesized_from_breakdowns() {
    let response =
        PromptResponse::new(StopReason::EndTurn).usage(Usage::new(1, 900, 800).thought_tokens(77));
    let normalized =
        AgentTurnUsage::from_prompt_response(AgentDiagnosticProvider::Codex, Some(&response));

    assert_eq!(
        normalized.provider_total_tokens().map(TokenCount::as_str),
        Some("1")
    );
    assert_eq!(
        normalized.input_tokens().map(TokenCount::as_str),
        Some("900")
    );
    assert_eq!(
        normalized.output_tokens().map(TokenCount::as_str),
        Some("800")
    );
    assert_eq!(
        normalized.thought_tokens().map(TokenCount::as_str),
        Some("77")
    );
}

#[test]
fn settlement_boundary_selects_each_unavailable_reason_uniquely() {
    let prompt_not_submitted = AgentTurnUsage::prompt_not_submitted();
    let settlement_unknown = AgentTurnUsage::turn_settlement_unknown();
    let source_unsupported = AgentTurnUsage::from_prompt_response(
        AgentDiagnosticProvider::Kimi,
        Some(&PromptResponse::new(StopReason::EndTurn).usage(Usage::new(3, 2, 1))),
    );
    let not_reported = AgentTurnUsage::from_prompt_response(AgentDiagnosticProvider::Codex, None);

    let reasons = [
        prompt_not_submitted,
        settlement_unknown,
        source_unsupported,
        not_reported,
    ]
    .map(|usage| usage.unavailable_reason().unwrap().as_str())
    .into_iter()
    .collect::<BTreeSet<_>>();
    assert_eq!(
        reasons,
        BTreeSet::from([
            "prompt_not_submitted",
            "source_unsupported",
            "turn_settlement_unknown",
            "usage_not_reported",
        ])
    );
}

#[test]
fn caller_cancellation_keeps_usage_owned_by_later_supervisor_settlement() {
    for provider in [
        AgentDiagnosticProvider::Codex,
        AgentDiagnosticProvider::Claude,
    ] {
        let response = PromptResponse::new(StopReason::Cancelled)
            .usage(Usage::new(13, 8, 5).thought_tokens(0).cached_read_tokens(2));
        let normalized = AgentTurnUsage::from_prompt_response(provider, Some(&response));
        assert_eq!(normalized.availability(), UsageAvailability::Available);
        assert_eq!(
            normalized.provider_total_tokens().map(TokenCount::as_str),
            Some("13")
        );
    }

    let turn = source(crate_root().join("src/session/turn.rs"));
    let request_cancel = &turn[turn.find("    pub fn request_cancel(&self)").unwrap()
        ..turn
            .find("    pub(crate) fn caller_cancellation(&self)")
            .unwrap()];
    let submitted = &request_cancel[request_cancel
        .find("AgentTurnControlPhase::Submitted => {")
        .unwrap()..];
    let submitted = &submitted[..submitted
        .find("\n            };\n            (accepted")
        .unwrap()];
    assert!(submitted.contains("AgentTurnControlPhase::SupervisorOwnedCancelled"));
    assert!(submitted.contains("supervisor_handoff = true"));
    assert!(!submitted.contains("observe_turn_terminal_locked"));

    let completion = &turn[turn.find("    fn complete_response(").unwrap()
        ..turn.find("    fn publish_caller_outcome(&self)").unwrap()];
    let retained = completion
        .find("retain_prompt_response_for_diagnostics")
        .unwrap();
    let terminal = completion
        .find("TurnTerminalObservation::settled(response, adapter)")
        .unwrap();
    assert!(completion.contains("AgentTurnControlPhase::SupervisorOwnedCancelled"));
    assert!(retained < terminal);
}

#[test]
fn candidate_is_typed_precanonical_and_source_boundary_has_no_derivation_seam() {
    fn assert_candidate<T: AgentDiagnosticCandidate>() {}
    assert_candidate::<AgentTurnUsageCandidate>();
    assert_eq!(AGENT_TURN_USAGE_CANDIDATE_KIND, "agent_turn_usage_terminal");
    assert_eq!(
        UsageSource::AcpPromptResponseUsage.as_str(),
        ACP_TURN_USAGE_SOURCE
    );
    assert_eq!(AgentUsageQualification::WholeTurn.as_str(), "whole_turn");

    let usage = source(crate_root().join("src/diagnostics/usage.rs"));
    assert_eq!(usage.matches("response.usage.as_ref()").count(), 1);
    assert_eq!(
        usage
            .matches("AgentDiagnosticObservation::Candidate")
            .count(),
        1
    );
    assert!(usage.contains("fn token_count_from_acp(value: u64) -> TokenCount"));
    assert!(!usage.contains("Option<u64>"));
    for forbidden in [
        "UsageUpdate",
        "context_used_tokens",
        "context_window_tokens",
        "tokenizer",
        "session_counter",
        "session counter",
        "std::fs",
        "read_to_string",
        "response.meta",
        "usage.meta",
        ".stop_reason",
        "checked_sub",
        "saturating_sub",
        "ActTokenUsageFinalized",
        "DiagnosticEventHeader",
        "serde_json",
    ] {
        assert!(
            !usage.contains(forbidden),
            "usage source boundary must not contain {forbidden}"
        );
    }
    for forbidden in [
        "total_tokens =",
        "input_tokens + output_tokens",
        "input_tokens.checked_add",
    ] {
        assert!(
            !usage.contains(forbidden),
            "provider total must not be synthesized through {forbidden}"
        );
    }

    assert!(repository_root().join("tests/fixtures/acp/usage").is_dir());
}
