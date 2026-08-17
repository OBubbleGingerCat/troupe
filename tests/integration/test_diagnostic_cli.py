from __future__ import annotations

import json
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
APPLICATION = ROOT / "rust" / "src" / "application"
CLI = APPLICATION / "cli.rs"
INVOCATION = APPLICATION / "invocation.rs"
DISPATCH = APPLICATION / "diagnostic_cli" / "dispatch.rs"
FIXTURES = ROOT / "tests" / "fixtures" / "diagnostics" / "cli"
ARTIFACT = ROOT / "tests" / "fixtures" / "artifact_layout" / "nodes" / "D07.json"
GATE = ROOT / "tests" / "fixtures" / "diagnostic_node_gates" / "D07.json"


def _read(path: Path) -> str:
    return path.read_text(encoding="utf-8")


def test_top_level_branch_returns_before_the_production_loader() -> None:
    source = _read(CLI)

    diagnostic_branch = "Invocation::Diagnostic(command) => return run_diagnostic(py, command)"
    assert diagnostic_branch in source
    assert source.index(diagnostic_branch) < source.index("activation::activate")
    for required in (
        "DiagnosticSignalGuard::install",
        "Builder::new_current_thread()",
        "py.detach",
        "py.check_signals()",
        "DiagnosticTermination",
        "error.line()",
    ):
        assert required in source


def test_invocation_preserves_legacy_production_arguments_and_validates_all_tokens() -> None:
    source = _read(INVOCATION)

    for required in (
        "TroupeInvocation::Production",
        "TroupeInvocation::Diagnostic",
        "RuntimeDiagnosticArgs",
        'token.eq("--")',
        "get_slice(index + 1, argv.len())",
        "fsencode",
        "fsdecode",
        "parse_encoded_arguments",
        "PRODUCTION_HELP",
    ):
        assert required in source
    assert "troupe run" in source
    assert "no `troupe run` subcommand is required" in source


def test_dispatch_connects_every_diagnostic_command_with_closed_exit_material() -> None:
    source = _read(DISPATCH)

    for variant in (
        "DiagnosticCommand::Runs",
        "DiagnosticCommand::Status",
        "DiagnosticCommand::Snapshot",
        "DiagnosticCommand::Events",
        "DiagnosticCommand::Dump",
        "DiagnosticCommand::Serve",
        "DiagnosticCommand::Cleanup",
    ):
        assert variant in source
    for required in (
        "DiagnosticTermination::Success",
        "DiagnosticTermination::Interrupted",
        "Self::Interrupted => 130",
        "COMMAND_FAILURE_PREFIX",
        "write_stdout_record",
        "write_stderr_line",
        "cleanup_apply::apply",
        "preview.satisfied()",
        "report.satisfied()",
    ):
        assert required in source
    for forbidden in ("load_production", "Production::", "println!", "eprintln!"):
        assert forbidden not in source


def test_frozen_help_surfaces_are_exact_and_have_no_run_subcommand() -> None:
    top = _read(FIXTURES / "help.txt")
    diagnostic = _read(FIXTURES / "help-diagnostic.txt")
    production = _read(FIXTURES / "help-run.txt")

    assert top.startswith("Run a Production or inspect its diagnostics\n\n")
    assert "troupe <COMMAND>" in top
    assert "  diagnostic  Inspect active and archived Production diagnostics" in top
    assert "  run " not in top
    for command in ("runs", "status", "snapshot", "events", "dump", "serve", "cleanup"):
        assert f"  {command}" in diagnostic
    assert "troupe --production <PACKAGE_DIR>" in production
    assert "no `troupe run` subcommand is required" in production


def test_d07_descriptors_are_realized_with_one_batched_source_gate() -> None:
    artifact = json.loads(_read(ARTIFACT))
    gate = json.loads(_read(GATE))

    assert artifact == {
        "state": "realized",
        "introduced": [
            "tests/fixtures/diagnostics/cli/help-diagnostic.txt",
            "tests/fixtures/diagnostics/cli/help-run.txt",
            "tests/fixtures/diagnostics/cli/help.txt",
            "tests/integration/test_diagnostic_cli.py",
        ],
        "modified": [
            "rust/src/application/cli.rs",
            "rust/src/application/diagnostic_cli/dispatch.rs",
            "rust/src/application/diagnostic_cli/mod.rs",
            "rust/src/application/invocation.rs",
            "rust/src/application/mod.rs",
        ],
        "removed": [],
        "generated": [],
    }
    assert gate == {
        "state": "realized",
        "argv": [
            [
                "pytest",
                "-q",
                "tests/integration/test_cli.py",
                "tests/integration/test_diagnostic_cli.py",
                "tests/unit/test_invocation.py",
            ],
            [
                "cargo",
                "test",
                "--locked",
                "--manifest-path",
                "rust/Cargo.toml",
                "--package",
                "troupe",
                "diagnostic_cli::dispatch",
            ],
        ],
        "env": {},
        "maturin_features": [],
        "cache_requirements": [],
        "exclusive_resources": [],
    }
