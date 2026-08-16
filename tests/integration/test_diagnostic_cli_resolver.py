from __future__ import annotations

from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
CLI_ROOT = ROOT / "rust" / "src" / "application" / "diagnostic_cli"
RUST_TEST = ROOT / "rust" / "tests" / "diagnostic_cli_resolver.rs"


def _read(path: Path) -> str:
    return path.read_text(encoding="utf-8")


def test_resolver_is_loader_free_and_does_not_reimplement_storage_or_protocol_codecs() -> None:
    sources = {
        name: _read(CLI_ROOT / name)
        for name in ("resolver.rs", "http_client.rs", "archive_target.rs")
    }
    combined = "\n".join(sources.values())

    for forbidden in (
        "load_production",
        "application::loader",
        "orchestration::production",
        "pyo3",
        "rusqlite",
        "serde_json",
        "SharedArchiveLease",
        "CleanupArchiveLease",
        "std::net::TcpStream",
    ):
        assert forbidden not in combined

    assert "DiagnosticReader::open_identified_archive" in sources["archive_target.rs"]
    assert "DiagnosticReader::open_archive" in sources["archive_target.rs"]
    assert "decode_server_identity" in sources["http_client.rs"]


def test_local_selection_uses_r02_classification_and_both_toctou_revalidations() -> None:
    source = _read(CLI_ROOT / "resolver.rs")

    for required in (
        "discover_with",
        "is_potentially_live",
        "revalidate_for_use",
        "revalidate_for_cleanup",
        "CandidateClassification::Active",
        "CandidateClassification::DefiniteStale",
        "classification.as_str()",
        "UnsafeCandidate",
        "AmbiguousLiveTarget",
        "AmbiguousArchiveTarget",
    ):
        assert required in source

    for forbidden_ordering in (
        "latest",
        "mtime",
        "modified()",
        "started_at",
        "ended_at",
        "sort_by_key",
        "sort_unstable",
    ):
        assert forbidden_ordering not in source


def test_http_target_is_bounded_identity_checked_and_http_s_capable() -> None:
    source = _read(CLI_ROOT / "http_client.rs")

    assert "reqwest" in source
    assert "Policy::none()" in source
    assert "MAX_IDENTITY_RESPONSE_BYTES" in source
    assert "response.chunk().await" in source
    assert "SERVER_PROTOCOL_VERSION" in source
    assert "RunIdentityMismatch" in source
    assert "LocatorIdentityMismatch" in source
    assert '"/api/v1/identity"' in source


def test_archive_target_keeps_reader_and_exposes_the_validated_prefix() -> None:
    source = _read(CLI_ROOT / "archive_target.rs")

    assert "reader: DiagnosticReader<'static>" in source
    assert "validated_watermark: SchemaU64" in source
    assert "captured.metadata().run_id()" in source
    assert "captured.captured_watermark()" in source
    assert "self.reader.capture()" in source


def test_rust_contract_exercises_resolution_matrix_without_real_network() -> None:
    source = _read(RUST_TEST)

    for test_name in (
        "copied_archive_uses_embedded_identity_captures_watermark_and_holds_q00_lease",
        "implicit_resolution_prefers_one_revalidated_active_run_over_archive_history",
        "potentially_live_ambiguity_and_unhealthy_explicit_run_never_bypass_to_sqlite",
        "explicit_definite_stale_run_revalidates_then_opens_only_its_same_id_archive",
        "locator_replacement_between_discovery_and_use_fails_closed",
        "implicit_archive_selection_requires_uniqueness_and_never_uses_latest_ordering",
        "implicit_selection_counts_only_q00_validated_stale_archives",
        "url_identity_uses_shared_strict_decoder_and_validates_protocol_run_and_base_path",
        "validated_registry_target_combines_local_authority_with_advertised_base_path",
        "active_revalidation_rejects_process_identity_change_before_returning_http_target",
    ):
        assert f"fn {test_name}()" in source

    assert "FakeProcesses" in source
    assert "FakeServers" in source
    assert "TcpListener" not in source
