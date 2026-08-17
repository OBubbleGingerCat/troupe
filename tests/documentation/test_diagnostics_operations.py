from __future__ import annotations

import json
import re
from pathlib import Path
from typing import Any


ROOT = Path(__file__).resolve().parents[2]
README = ROOT / "README.md"
EVENTS = ROOT / "docs/diagnostics/events.md"
OPERATIONS = ROOT / "docs/diagnostics/operations.md"
EVENT_SOURCE = ROOT / "rust/crates/troupe-diagnostics-core/src/event.rs"
REGISTRY_SOURCE = ROOT / "rust/crates/troupe-diagnostics-runtime/src/registry/publish.rs"
ARGS_SOURCE = ROOT / "rust/src/application/diagnostic_cli/args.rs"
STATUS_FIXTURE = ROOT / "tests/fixtures/diagnostics/cli/status-human.txt"
ARTIFACT = ROOT / "tests/fixtures/artifact_layout/nodes/O00.json"
GATE = ROOT / "tests/fixtures/diagnostic_node_gates/O00.json"


def _read(path: Path) -> str:
    return path.read_text(encoding="utf-8")


def _json(path: Path) -> Any:
    return json.loads(_read(path))


def _marked_fence(source: str, marker: str) -> str:
    pattern = rf"<!-- BEGIN {re.escape(marker)} -->\n```text\n(.*?)```\n<!-- END {re.escape(marker)} -->"
    match = re.search(pattern, source, flags=re.DOTALL)
    assert match is not None
    return match.group(1)


def test_overview_makes_the_mandatory_observation_boundary_visible() -> None:
    readme = _read(README)
    operations = _read(OPERATIONS)
    normalized = " ".join((readme + operations).split())

    for text in (
        "Every Production starts an in-process diagnostic server and a persistent event store",
        "There is no disable switch, alternate state root, memory-only fallback, or best-effort mode",
        "stops a running Production with a non-zero exit status",
            "Before importing or constructing Production code",
        "security_scope=\"trusted_network\"",
        "plain HTTP",
        "has no authentication, authorization, login, session, or credential mechanism",
        "does not inspect semantic content, find credentials, or redact captured values",
    ):
        assert text in normalized
    assert "docs/diagnostics/operations.md" in readme
    assert "docs/diagnostics/events.md" in readme


def test_state_layout_registry_permissions_and_ready_locator_match_sources() -> None:
    operations = _read(OPERATIONS)
    registry = _read(REGISTRY_SOURCE)
    arguments = _read(ARGS_SOURCE)

    for path in (
        ".troupe/",
        "diagnostics/",
        "instances/",
        "runs/",
        "<run-id>.json",
        "diagnostics.sqlite3",
        "diagnostics.sqlite3-wal",
        "diagnostics.sqlite3-shm",
    ):
        assert path in operations
    assert "REGISTRY_DIRECTORY_MODE: u32 = 0o700" in registry
    assert "REGISTRY_FILE_MODE: u32 = 0o600" in registry
    assert "`0700`" in operations
    assert "`0600`" in operations
    assert 'default_value = "0.0.0.0"' in arguments
    assert 'default_value = "0"' in arguments
    assert "`0.0.0.0`" in operations and "port `0`" in operations
    assert "troupe: diagnostic ready {" in operations
    assert "there is no singleton `active.json` or implicit latest Run" in operations


def test_network_proxy_and_failure_contracts_are_explicit() -> None:
    operations = _read(OPERATIONS)
    normalized = " ".join(operations.split())

    for header in (
        "`Forwarded`",
        "`X-Forwarded-Host`",
        "`X-Forwarded-Proto`",
        "`X-Forwarded-Prefix`",
    ):
        assert header in operations
    for failure in (
        "server execution-context exit",
        "unexpected listener close",
        "mandatory ingress exhaustion",
        "writer stall",
        "transaction or commit error",
        "disk or permission failure",
        "store invariant failure",
        "configured Run quota crossing",
    ):
        assert failure in normalized
    assert "All endpoints are read-only" in operations
    assert "single invalid request" in operations
    assert "optional Python sink callback" in normalized


def test_event_span_scope_gap_and_completeness_terms_match_the_model() -> None:
    document = _read(EVENTS)
    normalized = " ".join(document.split())
    event_source = _read(EVENT_SOURCE)

    for field in (
        "schema_version",
        "run_id",
        "sequence",
        "elapsed_ns",
        "scope",
        "caused_by",
        "scene_id",
        "actor_id",
        "cue_id",
        "effect_id",
        "act_id",
        "tool_call_id",
        "session_generation",
    ):
        assert f"`{field}`" in document
        assert field in event_source
    for term in (
        "dense prefix `1..W`",
        "start sequence is the stable `span_id`",
        "absent finish",
        "`ObservationGap`",
        "consumer-local loss",
        "`clean_shutdown=false`",
    ):
        assert term in normalized


def test_archive_lease_quota_retention_and_cleanup_are_closed() -> None:
    operations = _read(OPERATIONS)
    normalized = " ".join(operations.split())

    for term in (
        "active Run",
        "completed archive",
        "incomplete archive",
        "exclusive active archive lease",
        "shared leases",
        "exclusive cleanup lease",
        "retained indefinitely",
        "per-Run byte quota is unset by default",
        "cleanup is explicit and defaults to a preview",
        "an incomplete archive requires exact selection",
        "entire Run directory",
    ):
        assert term in normalized


def test_status_excerpt_is_the_checked_fixture() -> None:
    assert _marked_fence(_read(OPERATIONS), "DIAGNOSTIC STATUS FIXTURE") == _read(STATUS_FIXTURE)


def test_o00_descriptors_realize_only_the_operator_documentation_gate() -> None:
    assert _json(ARTIFACT) == {
        "state": "realized",
        "introduced": [
            "docs/diagnostics/events.md",
            "docs/diagnostics/operations.md",
            "tests/documentation/test_diagnostics_operations.py",
        ],
        "modified": ["README.md"],
        "removed": [],
        "generated": [],
    }
    assert _json(GATE) == {
        "state": "realized",
        "argv": [
            ["pytest", "-q", "tests/documentation/test_diagnostics_operations.py"],
            ["python", "-m", "doctest", "README.md"],
        ],
        "env": {},
        "maturin_features": [],
        "cache_requirements": [],
        "exclusive_resources": [],
    }
