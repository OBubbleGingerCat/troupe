from __future__ import annotations

import ast
import json
import re
from pathlib import Path
from typing import Any, Final


ROOT = Path(__file__).resolve().parents[2]
DOCUMENT = ROOT / "docs/diagnostics/index.md"
ARTIFACT = ROOT / "tests/fixtures/artifact_layout/nodes/O04.json"
GATE = ROOT / "tests/fixtures/diagnostic_node_gates/O04.json"
THIS_TEST = Path(__file__).resolve()

EXPECTED_LINKS: Final = (
    ("Operations and archives", "operations.md"),
    ("Canonical events", "events.md"),
    ("Python API", "python.md"),
    ("Live Web interface", "web.md"),
    ("Diagnostic CLI", "cli.md"),
    ("Perfetto export", "perfetto.md"),
    ("Release checklist", "RELEASE_CHECKLIST.md"),
)


def _read(path: Path) -> str:
    return path.read_text(encoding="utf-8")


def _json(path: Path) -> Any:
    return json.loads(_read(path))


def test_index_has_exact_closed_relative_links_and_every_target_exists() -> None:
    source = _read(DOCUMENT)
    links = tuple(re.findall(r"\[([^]]+)]\(([^)]+)\)", source))

    assert links == EXPECTED_LINKS
    for _, target in links:
        assert "://" not in target
        path = DOCUMENT.parent / target
        assert path.is_file()
        assert path.resolve(strict=True).parent == DOCUMENT.parent.resolve(strict=True)


def test_linked_titles_and_cross_page_v1_terms_remain_consistent() -> None:
    linked = {target: _read(DOCUMENT.parent / target) for _, target in EXPECTED_LINKS}
    assert linked["operations.md"].startswith("# Diagnostic operations\n")
    assert linked["events.md"].startswith("# Diagnostic events\n")
    assert linked["python.md"].startswith("# Python diagnostics API\n")
    assert linked["web.md"].startswith("# Live Web diagnostics\n")
    assert linked["cli.md"].startswith("# Diagnostic CLI\n")
    assert linked["perfetto.md"].startswith("# Perfetto trace export\n")
    assert linked["RELEASE_CHECKLIST.md"].startswith(
        "# Production Diagnostics Release Checklist\n"
    )

    assert "security_scope=\"trusted_network\"" in linked["operations.md"]
    assert "`DiagnosticEvent`" in linked["events.md"]
    assert "`DiagnosticSink`" in linked["python.md"]
    assert "Multiple Cues for one Actor" in linked["web.md"]
    assert "`troupe diagnostic`" in linked["cli.md"]
    assert "`publication_indeterminate`" in linked["perfetto.md"]
    assert "scripts/test_diagnostics_final.sh --all" in linked["RELEASE_CHECKLIST.md"]


def test_index_is_navigation_only_without_external_install_or_release_execution() -> None:
    document = _read(DOCUMENT)
    normalized = " ".join(document.split())

    for absent in (
        "pip install",
        "npm install",
        "npm ci",
        "node_modules",
        "ui.perfetto.dev",
        "scripts/test_diagnostics_final.sh",
        "publish_diagnostics_acceptance.py",
    ):
        assert absent not in normalized

    tree = ast.parse(_read(THIS_TEST), filename=str(THIS_TEST))
    imported = {
        alias.name
        for node in ast.walk(tree)
        if isinstance(node, (ast.Import, ast.ImportFrom))
        for alias in node.names
    }
    assert "subprocess" not in imported
    assert "runpy" not in imported


def test_o04_descriptors_own_only_the_index_and_static_bootstrap_gate() -> None:
    assert _json(ARTIFACT) == {
        "state": "realized",
        "introduced": [
            "docs/diagnostics/index.md",
            "tests/documentation/test_diagnostics_index.py",
        ],
        "modified": [],
        "removed": [],
        "generated": [],
    }
    assert _json(GATE) == {
        "state": "realized",
        "argv": [["pytest", "-q", "tests/documentation/test_diagnostics_index.py"]],
        "env": {},
        "maturin_features": [],
        "cache_requirements": [],
        "exclusive_resources": [],
    }
