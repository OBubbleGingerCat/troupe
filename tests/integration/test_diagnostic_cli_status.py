from __future__ import annotations

import json
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
STATUS_SOURCE = ROOT / "rust" / "src" / "application" / "diagnostic_cli" / "status.rs"
RUST_TEST = ROOT / "rust" / "tests" / "diagnostic_cli_status.rs"
FIXTURE_ROOT = ROOT / "tests" / "fixtures" / "diagnostics" / "cli"


def _read(path: Path) -> str:
    return path.read_text(encoding="utf-8")


def test_status_uses_h01_for_live_and_archive_after_d01_resolution() -> None:
    source = _read(STATUS_SOURCE)

    for required in (
        "resolve(target).await",
        "ResolvedDiagnosticTarget::Live",
        "ResolvedDiagnosticTarget::Archive",
        "get(STATUS_PATH)",
        "archive.capture()",
        "encode_status_response",
        "revalidate_identity()",
        "RunIdentityMismatch",
        "SourceMismatch",
    ):
        assert required in source

    for forbidden in (
        "load_production",
        "application::loader",
        "orchestration::production",
        "rusqlite",
        "serde_json",
        "super::snapshot",
        "super::runs",
    ):
        assert forbidden not in source


def test_status_response_is_bounded_strict_and_has_no_output_side_effects() -> None:
    source = _read(STATUS_SOURCE)

    for required in (
        "MAX_STATUS_RESPONSE_BYTES",
        "STATUS_REQUEST_TIMEOUT",
        "response.chunk().await",
        "Policy::none()",
        "exact_object",
        "is_canonical_u64",
        "MAX_JSON_DEPTH",
        'const SECURITY_SCOPE: &str = "trusted_network"',
        "output.push('\\n')",
    ):
        if required == "Policy::none()":
            assert required in _read(STATUS_SOURCE.with_name("http_client.rs"))
        else:
            assert required in source

    for forbidden_output in (
        "println!",
        "eprintln!",
        "std::io::stdout",
        "std::io::stderr",
    ):
        assert forbidden_output not in source


def test_versioned_json_fixture_preserves_uuid_decimal_null_and_unavailable() -> None:
    raw = (FIXTURE_ROOT / "status-v1.json").read_bytes()
    assert raw.endswith(b"\n")
    assert b"\n" not in raw[:-1]
    document = json.loads(raw)

    assert document["api_schema_version"] == 1
    assert document["run_id"] == "12345678-1234-4234-9234-123456789abc"
    assert document["source"] == "archive"
    assert document["security_scope"] == "trusted_network"
    for field in (
        "store_schema_version",
        "event_schema_version",
        "event_watermark",
        "read_model_watermark",
    ):
        assert isinstance(document[field], str)
        assert document[field].isdecimal()
    assert document["lifecycle"] == {
        "state": "incomplete",
        "started_at": "2026-08-16T00:00:00Z",
        "ended_at": None,
        "outcome": None,
        "clean_shutdown": False,
    }
    assert document["writer"] == {"status": "unavailable", "reason": "archive"}
    assert document["quota"] == {"status": "unavailable", "reason": "archive"}


def test_human_fixture_is_default_readable_and_complete() -> None:
    human = _read(FIXTURE_ROOT / "status-human.txt")
    args = _read(STATUS_SOURCE.with_name("args.rs"))

    assert "pub(crate) enum DocumentFormat" in args
    assert "#[default]\n    Human" in args
    for required in (
        "api_schema_version: 1",
        "run_id: 12345678-1234-4234-9234-123456789abc",
        "source: archive",
        "security_scope: trusted_network",
        "configuration_identity: configuration-sha256:d02",
        "event_watermark: 0",
        "read_model_watermark: 0",
        "state: incomplete",
        "clean_shutdown: false",
        "status: unavailable",
        "reason: archive",
    ):
        assert required in human


def test_rust_contract_covers_real_live_archive_and_operation_semantics() -> None:
    source = _read(RUST_TEST)

    for test_name in (
        "archive_status_uses_q00_semantics_and_matches_human_and_json_fixtures",
        "live_status_queries_h01_and_preserves_available_writer_quota_and_limits",
        "failed_and_incomplete_outcomes_are_documents_not_operation_errors",
        "protocol_identity_source_and_shape_errors_fail_closed",
        "captured_h01_response_run_identity_is_checked_against_the_resolved_server",
        "archive_store_and_resolver_failures_are_operation_errors",
    ):
        assert f"fn {test_name}()" in source

    assert "DiagnosticServer::start" in source
    assert "QueryEndpoints::active" in source
    assert "ActiveArchiveLease::acquire" in source
    assert "TcpListener" not in source
