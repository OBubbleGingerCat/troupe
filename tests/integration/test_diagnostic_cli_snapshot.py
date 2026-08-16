from __future__ import annotations

import json
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
SNAPSHOT_SOURCE = ROOT / "rust" / "src" / "application" / "diagnostic_cli" / "snapshot.rs"
RUST_TEST = ROOT / "rust" / "tests" / "diagnostic_cli_snapshot.rs"
FIXTURE_ROOT = ROOT / "tests" / "fixtures" / "diagnostics" / "cli"


def _read(path: Path) -> str:
    return path.read_text(encoding="utf-8")


def test_snapshot_reuses_d01_resolution_and_h01_for_live_and_archive() -> None:
    source = _read(SNAPSHOT_SOURCE)

    for required in (
        "resolve(target).await",
        "ResolvedDiagnosticTarget::Live",
        "ResolvedDiagnosticTarget::Archive",
        "get(SNAPSHOT_PATH)",
        "archive.capture()",
        "encode_snapshot_response",
        "revalidate_identity()",
        "RunIdentityMismatch",
    ):
        assert required in source

    for forbidden in (
        "load_production",
        "application::loader",
        "orchestration::production",
        "rusqlite",
        "query_events",
        "project_snapshot",
        "DiagnosticEvent",
        "ActTokenUsageFinalized",
    ):
        assert forbidden not in source


def test_snapshot_is_bounded_typed_strict_and_has_no_output_side_effects() -> None:
    source = _read(SNAPSHOT_SOURCE)

    for required in (
        "MAX_SNAPSHOT_RESPONSE_BYTES",
        "SNAPSHOT_REQUEST_TIMEOUT",
        "response.chunk().await",
        "SnapshotResponseV1",
        "SnapshotReadModel",
        "deny_unknown_fields",
        "serde_json::from_slice",
        "state.counters is inconsistent",
        "state.usage is inconsistent",
        "output.push('\\n')",
    ):
        assert required in source

    for forbidden_output in (
        "println!",
        "eprintln!",
        "std::io::stdout",
        "std::io::stderr",
    ):
        assert forbidden_output not in source


def test_versioned_json_fixture_preserves_uuid_decimal_null_and_read_models() -> None:
    raw = (FIXTURE_ROOT / "snapshot-v1.json").read_bytes()
    assert raw.endswith(b"\n")
    assert b"\n" not in raw[:-1]
    document = json.loads(raw)

    assert document["api_schema_version"] == 1
    assert document["run_id"] == "12345678-1234-4234-9234-123456789abc"
    assert document["watermark_sequence"] == "0"
    assert document["earliest_available_sequence"] is None

    state = document["state"]
    assert state["model_schema_version"] == 1
    assert state["run_id"] == document["run_id"]
    assert state["through_sequence"] == document["watermark_sequence"]
    assert state["through_elapsed_ns"] == "0"
    assert state["gaps"] == []
    assert state["truncations"] == []
    assert state["spans"]["spans"] == []
    assert state["messages"]["messages"] == []
    assert state["plans"]["plans"] == []
    assert state["counters"]["series"] == []
    assert state["usage"]["usages"] == []

    aggregate = state["usage"]["aggregate"]
    for count in (
        "finalized_acts",
        "reported_acts",
        "available_acts",
        "partial_acts",
        "unavailable_acts",
    ):
        assert aggregate[count] == "0"
    for tokens in (
        "provider_total_tokens",
        "input_tokens",
        "output_tokens",
        "thought_tokens",
        "cached_read_tokens",
        "cached_write_tokens",
    ):
        assert aggregate[tokens] == {
            "known_sum": None,
            "reported_acts": "0",
            "finalized_acts": "0",
        }


def test_human_fixture_is_default_readable_and_complete() -> None:
    human = _read(FIXTURE_ROOT / "snapshot-human.txt")
    args = _read(SNAPSHOT_SOURCE.with_name("args.rs"))

    assert "pub(crate) enum DocumentFormat" in args
    assert "#[default]\n    Human" in args
    for required in (
        "api_schema_version: 1",
        "run_id: 12345678-1234-4234-9234-123456789abc",
        "watermark_sequence: 0",
        "earliest_available_sequence: null",
        "spans: []",
        "messages: []",
        "plans: []",
        "series: []",
        "usages: []",
        "known_sum: null",
        "gaps: []",
        "truncations: []",
    ):
        assert required in human


def test_rust_contract_covers_single_w_two_cues_and_error_semantics() -> None:
    source = _read(RUST_TEST)

    for test_name in (
        "incomplete_archive_matches_human_and_versioned_json_fixtures",
        "live_and_archive_share_one_two_cue_materialized_read_model",
        "a_later_active_commit_does_not_pollute_the_captured_snapshot",
        "failed_and_incomplete_archives_are_readable_documents",
        "schema_identity_decimal_shape_and_size_errors_fail_closed",
        "archive_store_and_resolver_failures_are_operation_errors",
    ):
        assert f"fn {test_name}()" in source

    for required in (
        "DiagnosticServer::start",
        "QueryEndpoints::active_unobserved",
        "ActiveArchiveLease::acquire",
        "message-cue-1",
        "message-cue-2",
        r'"\"input_tokens\":\"42\""',
        r'"\"input_tokens\":\"84\""',
        r'"\"finalized_acts\":\"2\""',
        "encode_snapshot_response(run_id(), &captured)",
    ):
        assert required in source

    assert "TcpListener" not in source
