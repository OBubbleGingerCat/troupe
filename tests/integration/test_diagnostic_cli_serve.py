from __future__ import annotations

import json
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
SOURCE = ROOT / "rust" / "src" / "application" / "diagnostic_cli" / "serve.rs"
RUST_TEST = ROOT / "rust" / "tests" / "diagnostic_cli_serve.rs"
ARTIFACT = ROOT / "tests" / "fixtures" / "artifact_layout" / "nodes" / "D04.json"
GATE = ROOT / "tests" / "fixtures" / "diagnostic_node_gates" / "D04.json"


def _read(path: Path) -> str:
    return path.read_text(encoding="utf-8")


def test_serve_accepts_only_explicit_inactive_targets_through_d01() -> None:
    source = _read(SOURCE)
    target = _read(SOURCE.with_name("target.rs"))

    for required in (
        "arguments.into_parts()",
        "resolve_archive(target)",
        "resolve(target).await",
        "ServeTarget::Production",
        "run: Some(run)",
        "ServeTarget::Archive",
        "ResolvedDiagnosticTarget::Archive",
        "ResolvedDiagnosticTarget::Live",
        "ServeErrorCode::ActiveTarget",
    ):
        assert required in source

    assert "required_unless_present = \"archive\"" in target
    assert "requires = \"production\"" in target
    for forbidden in (
        "ServeTarget::Url",
        "run: None",
        "load_production",
        "application::loader",
        "orchestration::production",
        "rusqlite",
    ):
        assert forbidden not in source


def test_archive_server_composes_existing_read_only_owners_without_registry_or_sse() -> None:
    source = _read(SOURCE)

    for required in (
        "ArchiveRouteAssembly::new",
        "QueryEndpoints::archive",
        "ViewEndpoints::archive",
        "DumpEndpoints::archive",
        "PerfettoDumpProducer",
        "dump_captured_prefix(source, writer, through)",
        "DiagnosticServer::start",
        '.with_bind(LOOPBACK_BIND_HOST, port)',
        'const LOOPBACK_BIND_HOST: &str = "127.0.0.1"',
    ):
        assert required in source

    for forbidden in (
        "ActiveRouteAssembly",
        "SseEndpoint",
        "RegistryEntry",
        "publish_registry",
        "encode_registry_entry",
        "DiagnosticStore::create",
        "TransactionalWriter",
        "TcpListener::bind",
    ):
        assert forbidden not in source


def test_foreground_session_owns_lease_until_listener_shutdown_and_reports_locator() -> None:
    source = _read(SOURCE)

    for required in (
        "struct ArchiveServeSession",
        "archive: ArchiveTarget",
        "server: Option<DiagnosticServer>",
        "self.shutdown_server()",
        "session.run_until_cancelled(&cancellation).await",
        "server.try_core_failure()",
        "fs::canonicalize(archive.run_directory())",
        "ArchiveTarget::open_expected(&run_directory, run_id)",
        'pub(crate) const ARCHIVE_READY_PREFIX: &str = "troupe: diagnostic archive ready ";',
        "locator_schema_version",
        "run_id",
        "local_url",
        "archive_directory",
        "clean_shutdown",
    ):
        assert required in source

    assert source.index("self.shutdown_server()") < source.index(
        "Ok(ServeTermination::Interrupted)"
    )


def test_open_is_the_only_browser_side_effect_and_failure_is_a_warning() -> None:
    source = _read(SOURCE)

    assert "if open" in source
    assert "browser.launch(session.locator().local_url())" in source
    assert 'code: "browser_launch_failed"' in source
    assert "ARCHIVE_WARNING_PREFIX" in source
    assert "SystemBrowserLauncher" in source
    assert source.count(".spawn()") == 1
    for forbidden in (
        "webbrowser",
        "BrowserLauncher for ArchiveServeSession",
        "launch(session.locator().local_url())?",
        "println!",
        "eprintln!",
        "std::io::stdout",
    ):
        assert forbidden not in source


def test_rust_contract_covers_routes_lifecycle_active_rejection_and_browser_boundary() -> None:
    source = _read(RUST_TEST)

    for test_name in (
        "archive_server_reuses_full_read_only_routes_and_reports_incomplete_locator",
        "archive_server_holds_one_lifetime_shared_lease_and_releases_every_normal_exit",
        "open_is_the_only_browser_side_effect_and_launch_failure_is_nonfatal",
        "server_core_failure_stops_foreground_serve_and_releases_the_lease",
        "serve_rejects_a_revalidated_active_production_without_opening_a_browser",
        "occupied_explicit_port_fails_before_ready_and_releases_the_archive_lease",
    ):
        assert f"fn {test_name}()" in source

    for required in (
        '"/api/v1/identity"',
        '"/api/v1/status"',
        '"/api/v1/snapshot"',
        '"/api/v1/events?after=0"',
        '"/api/v1/views"',
        '"/api/v1/dump"',
        '"/"',
        "CleanupArchiveLease::acquire",
        "SharedArchiveLease::acquire",
        "ArchiveLeaseErrorCode::Contended",
        "current_process_identity()",
        "encode_registry_entry(&entry)",
    ):
        assert required in source


def test_d04_descriptors_are_realized_with_exact_direct_gate_and_no_build_extras() -> None:
    artifact = json.loads(_read(ARTIFACT))
    gate = json.loads(_read(GATE))

    assert artifact == {
        "state": "realized",
        "introduced": [
            "rust/tests/diagnostic_cli_serve.rs",
            "tests/integration/test_diagnostic_cli_serve.py",
        ],
        "modified": ["rust/src/application/diagnostic_cli/serve.rs"],
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
                "diagnostic_cli_serve",
            ],
            ["pytest", "-q", "tests/integration/test_diagnostic_cli_serve.py"],
        ],
        "env": {},
        "maturin_features": [],
        "cache_requirements": [],
        "exclusive_resources": [],
    }
