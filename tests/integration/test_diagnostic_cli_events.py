from __future__ import annotations

import json
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
EVENTS_SOURCE = (
    ROOT / "rust" / "src" / "application" / "diagnostic_cli" / "events_finite.rs"
)
ARGS_SOURCE = EVENTS_SOURCE.with_name("args.rs")
RUST_TEST = ROOT / "rust" / "tests" / "diagnostic_cli_events_finite.rs"
FIXTURE_ROOT = ROOT / "tests" / "fixtures" / "diagnostics" / "cli"
RUN_ID = "12345678-1234-4234-9234-123456789abc"


def _read(path: Path) -> str:
    return path.read_text(encoding="utf-8")


def test_events_reuses_d01_h01_and_q00_for_one_finite_capture() -> None:
    source = _read(EVENTS_SOURCE)

    for required in (
        "resolve(target).await",
        "ResolvedDiagnosticTarget::Live",
        "ResolvedDiagnosticTarget::Archive",
        "get(&path)",
        "EVENTS_PATH",
        "archive.capture()",
        "encode_events_response",
        "FiniteEventQuery::tail",
        "FiniteEventQuery::after",
        "revalidate_identity()",
        "RunIdentityMismatch",
    ):
        assert required in source

    assert source.count("archive.capture()") == 1
    for forbidden in (
        "load_production",
        "application::loader",
        "orchestration::production",
        "rusqlite",
        "query_events(",
        "project_snapshot",
        "Last-Event-ID",
        "text/event-stream",
        "EventSource",
        "reconnect",
        "aggregate",
    ):
        assert forbidden not in source


def test_events_is_bounded_typed_strict_and_has_no_output_side_effects() -> None:
    source = _read(EVENTS_SOURCE)

    for required in (
        "MAX_EVENTS_RESPONSE_BYTES",
        "EVENTS_REQUEST_TIMEOUT",
        "response.chunk().await",
        "EventsResponseV1",
        "Vec<DiagnosticEvent>",
        "deny_unknown_fields",
        "serde_json::from_slice",
        "serde_json::to_string(event)",
        "strictly increasing duplicate-free response",
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


def test_jsonl_fixture_is_exact_canonical_duplicate_free_event_stream() -> None:
    raw = (FIXTURE_ROOT / "events-v1.jsonl").read_bytes()
    assert raw.endswith(b"\n")
    assert b"\r" not in raw

    lines = raw.decode("utf-8").splitlines()
    assert len(lines) == 2
    assert all(lines)
    events = [json.loads(line) for line in lines]
    assert [json.dumps(event, ensure_ascii=False, separators=(",", ":")) for event in events] == lines

    sequences = [int(event["sequence"]) for event in events]
    assert sequences == sorted(sequences)
    assert len(sequences) == len(set(sequences))
    assert sequences == [1, 2]
    assert {event["run_id"] for event in events} == {RUN_ID}
    assert {event["schema_version"] for event in events} == {1}

    usage, counter = events
    assert usage["kind"] == "act_token_usage_finalized"
    assert usage["provider_total_tokens"] == (
        "1234567890123456789012345678901234567890"
    )
    assert usage["unavailable_reason"] is None
    assert usage["scope"] == {
        "scene_id": "scene-1",
        "actor_id": "actor-1",
        "cue_id": "cue-1",
        "effect_id": None,
        "act_id": "act-1",
        "tool_call_id": None,
        "session_generation": "1",
    }
    assert counter["kind"] == "counter_sampled"
    assert counter["value"] == "18446744073709551615"


def test_human_fixture_is_default_readable_and_preserves_event_context() -> None:
    human = _read(FIXTURE_ROOT / "events-human.txt")
    args = _read(ARGS_SOURCE)

    assert '#[arg(long, value_enum, default_value = "human")]' in args
    assert "EventStart::Tail(Count::new(100))" in args
    for required in (
        "api_schema_version: 1",
        f"run_id: {RUN_ID}",
        "captured_watermark: 2",
        "kind: act_token_usage_finalized",
        "kind: counter_sampled",
        "scene_id: scene-1",
        "actor_id: actor-1",
        "cue_id: cue-1",
        "act_id: act-1",
        "provider_total_tokens: 1234567890123456789012345678901234567890",
        "value: 18446744073709551615",
        "next_after: null",
    ):
        assert required in human


def test_rust_contract_covers_finite_queries_capture_and_typed_failures() -> None:
    source = _read(RUST_TEST)

    for test_name in (
        "archive_default_tail_matches_human_and_canonical_jsonl_fixtures",
        "live_and_archive_share_tail_after_zero_and_explicit_query_semantics",
        "one_captured_head_excludes_later_commits_and_preserves_canonical_bytes",
        "full_u64_cursor_domain_is_total_without_overflow",
        "failed_and_incomplete_archives_remain_readable_and_follow_is_not_finite",
        "identity_schema_cursor_order_duplicate_shape_and_size_errors_fail_closed",
        "archive_resolver_and_corrupt_store_fail_as_typed_operations",
    ):
        assert f"fn {test_name}()" in source

    for required in (
        "DiagnosticServer::start",
        "QueryEndpoints::active_unobserved",
        "ActiveArchiveLease::acquire",
        "EventStart::Tail(Count::new(0))",
        "EventStart::After(CanonicalU64::new(0))",
        "EventStart::After(CanonicalU64::new(u64::MAX))",
        "encode_events_response(",
        "commit W=2",
        "advance active head to W=3",
    ):
        assert required in source

    assert "TcpListener" not in source
