from __future__ import annotations

import json
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
SOURCE = ROOT / "rust" / "src" / "application" / "diagnostic_cli" / "dump.rs"
PERFETTO_SOURCE = (
    ROOT / "rust" / "crates" / "troupe-diagnostics-perfetto" / "src" / "dump.rs"
)
RUST_TEST = ROOT / "rust" / "tests" / "diagnostic_cli_dump.rs"
ARTIFACT = ROOT / "tests" / "fixtures" / "artifact_layout" / "nodes" / "D06.json"
GATE = ROOT / "tests" / "fixtures" / "diagnostic_node_gates" / "D06.json"


def _read(path: Path) -> str:
    return path.read_text(encoding="utf-8")


def test_dump_reuses_only_the_owned_target_stream_and_publication_boundaries() -> None:
    source = _read(SOURCE)

    for required in (
        "resolve(target)",
        "ResolvedDiagnosticTarget::Archive",
        "ResolvedDiagnosticTarget::Live",
        "ArchiveTarget",
        "dump_captured_prefix",
        "CapturedEventSource",
        "TraceStreamProducer",
        "publish_atomic_trace",
        "DiagnosticHttpClient",
        "DUMP_PATH",
    ):
        assert required in source

    for forbidden in (
        "Production::",
        "load_production",
        "application::loader",
        "DiagnosticStore::create",
        "TransactionalWriter",
        "TcpListener::bind",
        "Command::new",
    ):
        assert forbidden not in source


def test_archive_and_live_paths_capture_once_then_publish_on_the_cli_machine() -> None:
    source = _read(SOURCE)
    perfetto_source = _read(PERFETTO_SOURCE)

    assert "let source = archive" in source
    assert ".capture()" in source
    assert "LocalDumpProducer" in source
    assert "RemoteDumpProducer::new(client, through)" in source
    assert ".revalidate_identity()" in source
    assert source.count(".revalidate_identity()") == 2
    assert ".write_all(&chunk)" in source
    assert "const MAX_REMOTE_CHUNK_BYTES: usize = 64 * 1024" in source
    assert "TraceBodyValidator" in source
    assert "metadata.trace_metadata()" in source
    assert '"body_invalid"' in perfetto_source
    assert '"body_metadata_mismatch"' in perfetto_source
    assert "publish(output, force, cancellation" in source

    for forbidden in (
        "server_output",
        "remote_output",
        "upload",
        "ui.perfetto.dev",
        "trace_processor_shell",
        "webbrowser",
    ):
        assert forbidden not in source


def test_remote_contract_revalidates_identity_and_all_dump_metadata() -> None:
    source = _read(SOURCE)

    for required in (
        "PERFETTO_TRACE_MIME",
        "DUMP_RUN_ID_HEADER",
        "DUMP_CAPTURED_WATERMARK_HEADER",
        "DUMP_EXPORTED_THROUGH_HEADER",
        "DUMP_API_SCHEMA_VERSION_HEADER",
        "DUMP_EVENT_SCHEMA_VERSION_HEADER",
        "DUMP_EXPORTER_SCHEMA_VERSION_HEADER",
        "DUMP_TROUPE_VERSION_HEADER",
        "DUMP_PRODUCTION_OUTCOME_HEADER",
        "DUMP_CLEAN_SHUTDOWN_HEADER",
        "DUMP_CONTENT_WARNING_HEADER",
        "headers.contains_key(reqwest::header::CONTENT_ENCODING)",
        '"empty_body"',
        '"metadata_mismatch"',
    ):
        assert required in source

    assert source.index('remote_error("identity_before_request"') < source.index(
        ".send()"
    )
    assert source.index('remote_error("identity_after_body"') > source.index(
        "if !received_body"
    )


def test_atomic_outcomes_and_sigint_are_reported_without_stdout_or_false_cleanup_claims() -> None:
    source = _read(SOURCE)

    for required in (
        "PublicationState::Published",
        "PublicationState::NotPublished",
        "PublicationState::PublicationIndeterminate",
        "manual_check_paths",
        "PublicationObservationRecord",
        "failure_code",
        "uncertainty",
        "DumpTermination::Interrupted",
        "PublicationFailure::Cancelled",
        "PublicationState::Published if interrupted",
        'format!("troupe: diagnostic dump {encoded}\\n")',
        "report.paths().temp()",
        "report.paths().backup()",
    ):
        assert required in source

    assert "write_stderr" in source
    for forbidden in (
        "write_stdout",
        "println!",
        "eprintln!",
        "remove_file",
        "remove_dir_all",
    ):
        assert forbidden not in source


def test_rust_contract_covers_real_local_remote_policy_metadata_and_cancellation() -> None:
    source = _read(RUST_TEST)

    for test_name in (
        "local_archive_default_and_through_zero_publish_real_residue_free_traces",
        "existing_file_force_directory_and_symlink_follow_atomic_publisher_policy",
        "sigint_before_publication_reports_130_and_leaves_no_partial_target",
        "active_url_uses_h05_stream_and_publishes_on_the_callers_filesystem",
        "malformed_remote_body_is_not_published",
        "empty_remote_trace_packet_is_not_published",
        "remote_trace_metadata_mismatch_is_not_published",
        "sigint_during_remote_body_waits_for_atomic_cleanup_and_keeps_server_alive",
        "remote_metadata_rejects_identity_schema_watermark_and_transport_mismatch",
    ):
        assert f"fn {test_name}()" in source

    for required in (
        "DumpEndpoints::active",
        "ActiveArchiveLease::acquire",
        "CleanupArchiveLease::acquire",
        "ArchiveLeaseErrorCode::Contended",
        "assert_no_publication_residue",
        "Some(0)",
        "Some(3)",
        "CancellationToken::new()",
        "target_type_rejected",
        "target_already_exists",
    ):
        assert required in source


def test_d06_descriptors_are_realized_with_one_direct_batched_gate() -> None:
    artifact = json.loads(_read(ARTIFACT))
    gate = json.loads(_read(GATE))

    assert artifact == {
        "state": "realized",
        "introduced": [
            "rust/tests/diagnostic_cli_dump.rs",
            "tests/integration/test_diagnostic_cli_dump.py",
        ],
        "modified": ["rust/src/application/diagnostic_cli/dump.rs"],
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
                "diagnostic_cli_dump",
            ],
            ["pytest", "-q", "tests/integration/test_diagnostic_cli_dump.py"],
        ],
        "env": {},
        "maturin_features": [],
        "cache_requirements": [],
        "exclusive_resources": [],
    }
