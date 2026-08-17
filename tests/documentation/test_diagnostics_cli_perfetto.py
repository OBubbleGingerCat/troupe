from __future__ import annotations

import json
import re
from pathlib import Path
from typing import Any


ROOT = Path(__file__).resolve().parents[2]
CLI_DOCUMENT = ROOT / "docs/diagnostics/cli.md"
PERFETTO_DOCUMENT = ROOT / "docs/diagnostics/perfetto.md"
ARGS = ROOT / "rust/src/application/diagnostic_cli/args.rs"
TARGET = ROOT / "rust/src/application/diagnostic_cli/target.rs"
DISPATCH = ROOT / "rust/src/application/diagnostic_cli/dispatch.rs"
DUMP = ROOT / "rust/src/application/diagnostic_cli/dump.rs"
PROJECT = ROOT / "rust/crates/troupe-diagnostics-perfetto/src/project.rs"
COLLECT = ROOT / "rust/crates/troupe-diagnostics-perfetto/src/collect.rs"
ATOMIC = ROOT / "rust/crates/troupe-diagnostics-perfetto/src/atomic_file.rs"
HELP_FIXTURE = ROOT / "tests/fixtures/diagnostics/cli/help-diagnostic.txt"
ARTIFACT = ROOT / "tests/fixtures/artifact_layout/nodes/O03.json"
GATE = ROOT / "tests/fixtures/diagnostic_node_gates/O03.json"


def _read(path: Path) -> str:
    return path.read_text(encoding="utf-8")


def _json(path: Path) -> Any:
    return json.loads(_read(path))


def _marked_fence(source: str, marker: str) -> str:
    pattern = rf"<!-- BEGIN {re.escape(marker)} -->\n```text\n(.*?)```\n<!-- END {re.escape(marker)} -->"
    match = re.search(pattern, source, flags=re.DOTALL)
    assert match is not None
    return match.group(1)


def test_help_gate_cli_grammar_targets_defaults_and_no_import_boundary() -> None:
    document = _read(CLI_DOCUMENT)
    normalized = " ".join(document.split())
    arguments = _read(ARGS)
    target = _read(TARGET)

    for command in ("runs", "status", "snapshot", "events", "dump", "serve", "cleanup"):
        assert f"troupe diagnostic {command}" in document
        assert f"{command.title()}(" in arguments
    for selector in (
        "--production PROD [--run RUN_ID]",
        "--url BASE_URL",
        "--archive RUN_DIRECTORY",
    ):
        assert selector in document
    assert "exactly one of these mutually exclusive selectors" in document
    assert "`--run` requires `--production`" in document
    assert "complete Run directory, not a bare `diagnostics.sqlite3` file" in document
    assert "With neither start option, `events` means `--tail 100`" in document
    assert "default_value = \"human\"" in arguments
    assert "EventStart::Tail(Count::new(100))" in arguments
    assert 'default_value = "0"' in arguments
    assert "without importing the Production package" in normalized
    assert "ServeTarget" in target and "DiagnosticTarget" in target


def test_help_gate_machine_output_exit_status_serve_and_cleanup_are_exact() -> None:
    document = _read(CLI_DOCUMENT)
    dispatch = _read(DISPATCH)
    normalized = " ".join(document.split())

    for term in (
        "one newline-terminated versioned JSON document",
        "one unwrapped `DiagnosticEvent` per line",
        "Machine stdout contains only requested data",
        "`0` | Command completed successfully",
        "`1` | Discovery, server, protocol, store, export, or cleanup operation failed",
        "`2` | Command-line usage or argument validation failed",
        "`130` | The user interrupted the command",
        "loopback only",
        "OS-assigned `--port 0`",
        "It is a preview unless `--apply` is explicit",
        "requires an exclusive cleanup lease",
    ):
        assert term in normalized
    assert "Self::Interrupted => 130" in dispatch
    assert "COMMAND_FAILURE_PREFIX" in dispatch
    assert "cleanup_apply::apply" in dispatch
    assert "preview.satisfied()" in dispatch


def test_help_gate_command_excerpt_is_the_frozen_fixture() -> None:
    assert _marked_fence(_read(CLI_DOCUMENT), "DIAGNOSTIC HELP FIXTURE") == _read(HELP_FIXTURE)


def test_help_gate_perfetto_capture_routes_watermark_and_lease_contract() -> None:
    document = _read(PERFETTO_DOCUMENT)
    dump = _read(DUMP)
    normalized = " ".join(document.split())

    for term in (
        "stable committed prefix",
        "captures committed watermark `W`",
        "T03 encoder",
        "T08 local atomic-file publisher",
        "`GET /api/v1/dump[?through=SEQ]`",
        "cannot name a server filesystem path",
        "affect only the caller's local filesystem",
        "borrows the Runtime's already-held active guard",
        "shared lease",
        "Neither path writes a trace on the server",
    ):
        assert term in normalized
    assert "ResolvedDiagnosticTarget::Archive" in dump
    assert "ResolvedDiagnosticTarget::Live" in dump
    assert "RemoteDumpProducer::new" in dump
    assert "publish_atomic_trace" in dump


def test_help_gate_atomic_publication_has_closed_three_state_recovery() -> None:
    document = _read(PERFETTO_DOCUMENT)
    atomic = _read(ATOMIC)
    normalized = " ".join(document.split())

    for state in ("`published`", "`not_published`", "`publication_indeterminate`"):
        assert state in document
    for source_state in ("Published", "NotPublished", "PublicationIndeterminate"):
        assert source_state in atomic
    for term in (
        "identity-checked regular file",
        "hard-link backup",
        "durably syncs that backup",
        "stable phase and observed target, temporary, and backup identities",
        "Do not automatically retry",
        "claim that the old target is unchanged",
    ):
        assert term in normalized


def test_help_gate_trace_precision_sensitivity_and_provenance_are_frozen() -> None:
    document = _read(PERFETTO_DOCUMENT)
    normalized = " ".join(document.split())
    project = _read(PROJECT)
    collect = _read(COLLECT)

    for term in (
        "clock ID `11`",
        "Run-relative `elapsed_ns`",
        "exact `int64`",
        "exact finite double",
        "troupe.counter_projection=\"not_exact\"",
        "message body text is not exported",
        "captured watermark",
        "exported-through watermark",
        "trace may contain sensitive diagnostic metadata and user-provided attributes",
        "official Perfetto v57.2",
        "da1d152cff27890903d158fe96751de3aab883cc",
        "three pinned offline layers",
    ):
        assert term in normalized
    assert '"troupe.counter_projection", "not_exact"' in project
    assert "TRACE_CONTENT_WARNING" in collect
    assert "PERFETTO_EXPORTER_SCHEMA_VERSION: u8 = 1" in collect


def test_help_gate_perfetto_is_offline_not_realtime_or_an_install_dependency() -> None:
    document = _read(PERFETTO_DOCUMENT)
    normalized = " ".join(document.split())

    for term in (
        "does not use Perfetto for the real-time Web interface",
        "does not embed Perfetto UI",
        "Open the resulting local file manually",
        "requires no user installation of Node, npm, Perfetto source, SDK, or protobuf compiler",
    ):
        assert term in normalized


def test_help_gate_o03_descriptors_realize_the_exact_cli_perfetto_gate() -> None:
    assert _json(ARTIFACT) == {
        "state": "realized",
        "introduced": [
            "docs/diagnostics/cli.md",
            "docs/diagnostics/perfetto.md",
            "tests/documentation/test_diagnostics_cli_perfetto.py",
        ],
        "modified": [],
        "removed": [],
        "generated": [],
    }
    assert _json(GATE) == {
        "state": "realized",
        "argv": [[
            "pytest",
            "-q",
            "tests/documentation/test_diagnostics_cli_perfetto.py",
            "tests/integration/test_diagnostic_cli.py",
            "-k",
            "help",
        ]],
        "env": {},
        "maturin_features": [],
        "cache_requirements": [],
        "exclusive_resources": [],
    }
