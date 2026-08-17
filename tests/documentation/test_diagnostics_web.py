from __future__ import annotations

import json
from pathlib import Path
from typing import Any


ROOT = Path(__file__).resolve().parents[2]
DOCUMENT = ROOT / "docs/diagnostics/web.md"
MODEL = ROOT / "frontend/diagnostics/src/state/model.ts"
TOOLBAR = ROOT / "frontend/diagnostics/src/shell/PrimaryToolbar.tsx"
COMPATIBILITY = ROOT / "frontend/diagnostics/src/protocol/compatibility.ts"
BOOTSTRAP = ROOT / "frontend/diagnostics/src/live/bootstrap.ts"
ARTIFACT = ROOT / "tests/fixtures/artifact_layout/nodes/O02.json"
GATE = ROOT / "tests/fixtures/diagnostic_node_gates/O02.json"


def _read(path: Path) -> str:
    return path.read_text(encoding="utf-8")


def _json(path: Path) -> Any:
    return json.loads(_read(path))


def test_live_and_archive_same_origin_hierarchy_keeps_actor_cues_distinct() -> None:
    document = _read(DOCUMENT)
    normalized = " ".join(document.split())

    for term in (
        "active Productions",
        "inactive archives",
        "from one origin",
        "Production, Scene, Actor, Cue, Act, and tool identity",
        "Actor investigator",
        "Cue c-102",
        "Cue c-103",
        "Cue c-104",
        "mailbox wait",
        "Multiple Cues for one Actor are never merged",
        "Different Actors can execute concurrently",
    ):
        assert term in normalized


def test_trace_transcript_tool_result_context_usage_and_views_are_documented() -> None:
    document = _read(DOCUMENT)
    toolbar = _read(TOOLBAR)

    for label in ("Timeline", "Agent", "Events", "Usage", "Views"):
        assert label in document
        assert f'"{label}"' in toolbar
    for term in (
        "hierarchical trace",
        "transcript",
        "tool activity",
        "result submission",
        "Live context occupancy",
        "Final Act accounting",
        "Timeline, Metric, Table, and TimeSeries",
        "does not execute Production Python",
    ):
        assert term in document


def test_sse_replay_reconnect_pause_and_resume_are_server_backed() -> None:
    document = _read(DOCUMENT)
    normalized = " ".join(document.split())

    for term in (
        "SSE stream",
        "`stream_ready`",
        "`(run_id, sequence)`",
        "`stream_closed`",
        "Pause freezes presentation, not ingestion or the Runtime",
        "number of unseen sequences",
        "Resume uses a server range query",
        "does not accumulate the whole paused raw stream",
    ):
        assert term in normalized


def test_browser_window_limits_are_checked_against_release_constants() -> None:
    document = _read(DOCUMENT)
    normalized = " ".join(document.split())
    model = _read(MODEL)
    expected = {
        "ADJACENT_WINDOW_CAPACITY": (4, "four adjacent windows"),
        "VISIBLE_WINDOW_EVENT_CAPACITY": (4_096, "4,096 events"),
        "LIVE_EDGE_EVENT_CAPACITY": (256, "256 live-edge events"),
        "SPAN_CAPACITY": (256, "256 spans"),
        "MESSAGE_CAPACITY": (128, "128 messages"),
        "TOOL_FACT_CAPACITY": (256, "256 tool facts"),
        "RESULT_FACT_CAPACITY": (256, "256 result facts"),
        "CONTEXT_USAGE_CAPACITY": (128, "128 context samples"),
        "ACT_USAGE_CAPACITY": (256, "256 final Act usage facts"),
        "GAP_CAPACITY": (128, "128 gaps"),
        "QUERY_RESULT_CAPACITY": (64, "64 query results"),
    }
    for name, (value, prose) in expected.items():
        source_value = f"{value:_}" if value >= 1_000 else str(value)
        assert f"export const {name} = {source_value};" in model
        assert prose in normalized


def test_compatibility_and_security_floor_fail_static_before_live_work() -> None:
    document = _read(DOCUMENT)
    normalized = " ".join(document.split())
    compatibility = _read(COMPATIBILITY)
    bootstrap = _read(BOOTSTRAP)

    for term in (
        "Chromium and Edge 111",
        "Firefox 115",
        "Safari 16.4",
        "native `fetch`, `EventSource`, and `BigInt`",
        "static compatibility surface",
        "security_scope=\"trusted_network\"",
        "same-origin relative routes",
        "rendered as text rather than HTML",
        "Content Security Policy",
        "X-Content-Type-Options: nosniff",
        "Referrer-Policy: no-referrer",
    ):
        assert term in normalized
    assert 'mode: incompatible || missingBrowserCapabilities.length > 0 ? "static" : "interactive"' in compatibility
    for capability in ('missing.push("fetch")', 'missing.push("EventSource")', 'missing.push("BigInt")'):
        assert capability in bootstrap


def test_web_document_stays_inside_its_read_only_frontend_boundary() -> None:
    document = _read(DOCUMENT).lower()
    for forbidden in (
        "perfetto",
        "oauth",
        "cors",
        "localstorage",
        "indexeddb",
        "service worker",
        "python api",
        "cli grammar",
    ):
        assert forbidden not in document


def test_o02_descriptors_realize_only_the_bootstrap_documentation_gate() -> None:
    assert _json(ARTIFACT) == {
        "state": "realized",
        "introduced": [
            "docs/diagnostics/web.md",
            "tests/documentation/test_diagnostics_web.py",
        ],
        "modified": [],
        "removed": [],
        "generated": [],
    }
    assert _json(GATE) == {
        "state": "realized",
        "argv": [["pytest", "-q", "tests/documentation/test_diagnostics_web.py"]],
        "env": {},
        "maturin_features": [],
        "cache_requirements": [],
        "exclusive_resources": [],
    }
