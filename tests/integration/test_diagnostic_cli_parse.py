from __future__ import annotations

from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
CLI_ROOT = ROOT / "rust" / "src" / "application" / "diagnostic_cli"
RUST_TEST = ROOT / "rust" / "tests" / "diagnostic_cli_args.rs"


def _read(path: Path) -> str:
    return path.read_text(encoding="utf-8")


def test_private_parser_slots_are_loader_free_and_wired_only_at_the_entry_point() -> None:
    sources = {
        name: _read(CLI_ROOT / name)
        for name in ("args.rs", "target.rs", "values.rs")
    }
    combined = "\n".join(sources.values())

    for forbidden in (
        "load_production",
        "application::loader",
        "pyo3",
        "std::fs",
        "reqwest",
        "diagnostic_cli::dispatch",
        "diagnostic_cli::resolver",
        "diagnostic_cli::http_client",
    ):
        assert forbidden not in combined

    module_root = _read(CLI_ROOT / "mod.rs")
    for module in ("args", "target", "values"):
        assert f"pub(crate) mod {module};" in module_root

    # D07 owns the application entry-point join. The D00 parser modules remain
    # independently testable and are consumed only by the two assembly modules.
    cli = _read(ROOT / "rust" / "src" / "application" / "cli.rs")
    invocation = _read(ROOT / "rust" / "src" / "application" / "invocation.rs")
    assert "use crate::application::diagnostic_cli" in cli
    assert "use crate::application::diagnostic_cli" in invocation
    assert "ParsedInvocation::Diagnostic(command)" in cli
    assert "TroupeInvocation::Diagnostic(command)" in invocation


def test_frozen_command_and_option_surface_is_explicit() -> None:
    source = _read(CLI_ROOT / "args.rs")
    for variant in (
        "Runs(RunsArgs)",
        "Status(StatusArgs)",
        "Snapshot(SnapshotArgs)",
        "Events(EventsArgs)",
        "Dump(DumpArgs)",
        "Serve(ServeArgs)",
        "Cleanup(CleanupArgs)",
    ):
        assert source.count(variant) == 1

    for runtime_flag in (
        "diagnostic-bind-host",
        "diagnostic-port",
        "diagnostic-advertise-url",
        "diagnostic-max-run-bytes",
        "diagnostic-writer-stall-timeout",
        "diagnostic-shutdown-timeout",
    ):
        assert source.count(f'long = "{runtime_flag}"') == 1

    for forbidden_flag in (
        "diagnostic-disable",
        "diagnostic-root",
        "diagnostic-auth",
        "diagnostic-queue",
        "diagnostic-batch",
        "diagnostic-retention",
    ):
        assert forbidden_flag not in source

    assert "default_value = \"10s\"" in source
    assert "default_value = \"30s\"" in source
    assert "EventStart::Tail(Count::new(100))" in source
    assert "conflicts_with = \"after\"" in source
    assert "conflicts_with = \"archive\"" in source
    assert "default_value = \"human\"" in source
    assert "Jsonl" in source


def test_target_and_value_contracts_are_closed_in_the_owned_modules() -> None:
    target = _read(CLI_ROOT / "target.rs")
    values = _read(CLI_ROOT / "values.rs")

    assert 'required_unless_present_any = ["url", "archive"]' in target
    assert 'conflicts_with_all = ["url", "archive"]' in target
    assert 'requires = "production"' in target
    assert 'required_unless_present = "archive"' in target
    assert "DiagnosticTarget::Production" in target
    assert "DiagnosticTarget::Url" in target
    assert "DiagnosticTarget::Archive" in target

    assert "CanonicalUuid::parse" in values
    assert "WebBaseUrl::parse" in values
    assert "BindEndpoint::new" in values
    for unit in ('"KiB"', '"MiB"', '"GiB"', '"TiB"'):
        assert unit in values
    for unit in ('"ms"', '"s"', '"m"', '"h"'):
        assert unit in values
    for unit in ('"h"', '"d"', '"w"'):
        assert unit in values
    assert "checked_mul" in values
    assert "value.starts_with('0')" in values


def test_rust_contract_contains_behavioral_no_loader_and_usage_code_proofs() -> None:
    source = _read(RUST_TEST)
    assert "diagnostic_parse_is_pure_and_never_loads_a_production" in source
    assert "loader-ran" in source
    assert "!marker.exists()" in source
    assert "assert_eq!(error.exit_code(), 2" in source
    assert "runtime_flags_are_closed_and_only_parsed_before_the_separator" in source
