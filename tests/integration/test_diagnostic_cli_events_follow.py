from __future__ import annotations

import json
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
FOLLOW_SOURCE = (
    ROOT / "rust" / "src" / "application" / "diagnostic_cli" / "events_follow.rs"
)
ARGS_SOURCE = FOLLOW_SOURCE.with_name("args.rs")
RUST_TEST = ROOT / "rust" / "tests" / "diagnostic_cli_events_follow.rs"
ARTIFACT_DESCRIPTOR = (
    ROOT / "tests" / "fixtures" / "artifact_layout" / "nodes" / "D10.json"
)
GATE_DESCRIPTOR = (
    ROOT / "tests" / "fixtures" / "diagnostic_node_gates" / "D10.json"
)


def _read(path: Path) -> str:
    return path.read_text(encoding="utf-8")


def test_follow_reuses_d03_finite_and_h02_control_protocol() -> None:
    source = _read(FOLLOW_SOURCE)

    for required in (
        "events_finite::query",
        "ResolvedDiagnosticTarget::Live(client.clone())",
        "initial.events()",
        "initial.captured_watermark()",
        "SseFrameKind::from_event_name",
        "decode_control_payload",
        "DecodedSseControl::StreamReady",
        "DecodedSseControl::Heartbeat",
        "DecodedSseControl::DeliveryGap",
        "DecodedSseControl::ResyncRequired",
        "DecodedSseControl::StreamClosed",
        "SSE_CONTENT_TYPE",
        "EVENTS_PATH",
    ):
        assert required in source

    for forbidden_protocol_copy in (
        "struct StreamReadyControl",
        "struct HeartbeatControl",
        "struct DeliveryGapControl",
        "struct ResyncRequiredControl",
        "struct StreamClosedControl",
        "control_schema_version:",
        "#[derive(Deserialize",
        "decode_initial_events",
        ".render(EventsFormat::Jsonl)",
        "validated finite event lost canonical encoding",
    ):
        assert forbidden_protocol_copy not in source


def test_follow_state_is_active_only_seamless_and_cursor_exact() -> None:
    source = _read(FOLLOW_SOURCE)

    for required in (
        "ArchiveUnsupported",
        "EventCursor",
        "run_id: CanonicalUuid",
        "sequence: SchemaU64",
        "LAST_EVENT_ID_HEADER",
        "is_temporary_identity_failure",
        "stream ended without stream_closed",
        "adopt_connection_head: is_tail_zero(start)",
        "state.cursor.sequence = control.replay_through()",
        "identity.sequence.get() <= self.cursor.sequence.get()",
        ".checked_add(1)",
        "ConnectionOutcome::Reconnect",
        "FollowErrorCode::ResyncRequired",
        "ConnectionOutcome::Closed",
    ):
        assert required in source

    for forbidden in (
        "ResolvedDiagnosticTarget::Archive(mut",
        "DiagnosticReader",
        "rusqlite",
        "SseEndpoint",
        "open_subscriber",
        "ProductionDiagnosticHub",
    ):
        assert forbidden not in source


def test_controls_never_write_stdout_and_product_has_no_io_side_effects() -> None:
    source = _read(FOLLOW_SOURCE)

    assert source.count("write_stdout_record(&record)") == 1
    assert "write_event(output, &event, format)?" in source
    assert "write_stderr_line(&format!(" in source
    for forbidden_output in (
        "println!",
        "eprintln!",
        "std::io::stdout",
        "std::io::stderr",
        "tokio::signal",
    ):
        assert forbidden_output not in source


def test_sigint_contract_is_typed_130_with_complete_record_prefix() -> None:
    source = _read(FOLLOW_SOURCE)
    rust_test = _read(RUST_TEST)

    for required in (
        "CancellationToken",
        "FollowTermination::Interrupted",
        "Self::Interrupted => 130",
        "record.push('\\n')",
        "Writes one complete event record",
    ):
        assert required in source
    for required in (
        "cancellation_is_exit_130_and_leaves_a_valid_jsonl_prefix",
        "assert_eq!(termination.exit_code(), 130)",
        "serde_json::from_str(line).expect(\"valid JSONL event\")",
        "assert!(output.captured.stdout.ends_with('\\n'))",
    ):
        assert required in rust_test


def test_rust_contract_covers_the_frozen_follow_matrix() -> None:
    rust_test = _read(RUST_TEST)

    for test_name in (
        "finite_prefix_temporary_disconnect_reconnect_and_dedupe_are_seamless",
        "tail_zero_empty_finite_at_nonzero_head_adopts_connection_head",
        "delivery_gap_reconnects_from_last_output_and_replays_missing_range",
        "resync_and_stream_identity_change_fail_closed",
        "cancellation_is_exit_130_and_leaves_a_valid_jsonl_prefix",
        "human_records_are_complete_and_strictly_increasing",
        "archive_follow_is_rejected_by_the_shared_cli_grammar",
    ):
        assert f"fn {test_name}()" in rust_test

    for required in (
        "DiagnosticServer::start",
        "QueryEndpoints::active_unobserved",
        "SseFrame::stream_ready",
        "SseFrame::delivery_gap",
        "SseFrame::resync_required",
        "CursorSource::LastEventId",
        "jsonl_sequences(&output.stdout)",
        "ErrorKind::ArgumentConflict",
    ):
        assert required in rust_test


def test_descriptors_realize_only_the_frozen_d10_paths_and_commands() -> None:
    artifact = json.loads(_read(ARTIFACT_DESCRIPTOR))
    gate = json.loads(_read(GATE_DESCRIPTOR))

    assert artifact == {
        "state": "realized",
        "introduced": [
            "rust/tests/diagnostic_cli_events_follow.rs",
            "tests/integration/test_diagnostic_cli_events_follow.py",
        ],
        "modified": ["rust/src/application/diagnostic_cli/events_follow.rs"],
        "removed": [],
        "generated": [],
    }
    assert gate == {
        "state": "realized",
        "argv": [
            [
                "cargo",
                "test",
                "--locked",
                "--manifest-path",
                "rust/Cargo.toml",
                "--package",
                "troupe",
                "--test",
                "diagnostic_cli_events_follow",
            ],
            ["pytest", "-q", "tests/integration/test_diagnostic_cli_events_follow.py"],
        ],
        "env": {},
        "maturin_features": [],
        "cache_requirements": [],
        "exclusive_resources": [],
    }
    assert '#[arg(long, conflicts_with = "archive")]' in _read(ARGS_SOURCE)
