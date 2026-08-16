from __future__ import annotations

import json
import re
from pathlib import Path
from typing import Any


ROOT = Path(__file__).resolve().parents[2]
FIXTURES = ROOT / "tests" / "fixtures" / "diagnostics" / "cli"
UUID_RE = re.compile(
    r"[0-9a-f]{8}-[0-9a-f]{4}-[1-5][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}"
)
DECIMAL_RE = re.compile(r"0|[1-9][0-9]*")

CANDIDATE_FIELDS = {
    "classification",
    "source",
    "run_id",
    "path_run_id",
    "path",
    "archive_directory",
    "archive_present",
    "detail",
    "registry",
}
REGISTRY_FIELDS = {
    "registry_schema_version",
    "server_protocol_version",
    "run_id",
    "archive_directory",
    "owner_pid",
    "process_identity",
    "bind_host",
    "port",
    "local_endpoint",
    "advertise_url",
    "security_scope",
    "started_at",
}
CLASSIFICATIONS = {
    "active",
    "definite_stale",
    "unhealthy",
    "identity_mismatch",
    "invalid",
    "incompatible",
    "completed",
    "incomplete",
}


def _json_document() -> tuple[bytes, dict[str, Any]]:
    raw = (FIXTURES / "runs-v1.json").read_bytes()
    document = json.loads(raw)
    assert isinstance(document, dict)
    return raw, document


def test_runs_v1_is_one_newline_terminated_versioned_machine_document() -> None:
    raw, document = _json_document()

    assert raw.endswith(b"\n")
    assert raw.count(b"\n") == 1
    assert set(document) == {
        "runs_schema_version",
        "production",
        "candidate_count",
        "candidates",
    }
    assert document["runs_schema_version"] == 1
    assert document["production"] == "/srv/troupe/production"
    assert DECIMAL_RE.fullmatch(document["candidate_count"])
    assert int(document["candidate_count"]) == len(document["candidates"])


def test_runs_v1_lists_every_classification_and_untrusted_path_deterministically() -> None:
    _, document = _json_document()
    candidates = document["candidates"]

    assert {candidate["classification"] for candidate in candidates} == CLASSIFICATIONS
    assert [candidate["path"] for candidate in candidates] == sorted(
        candidate["path"] for candidate in candidates
    )
    assert {candidate["source"] for candidate in candidates} == {"instance", "archive"}

    for candidate in candidates:
        assert set(candidate) == CANDIDATE_FIELDS
        assert isinstance(candidate["archive_present"], bool)
        for field in ("run_id", "path_run_id"):
            value = candidate[field]
            assert value is None or UUID_RE.fullmatch(value)
        assert candidate["archive_directory"] is None or isinstance(
            candidate["archive_directory"], str
        )
        assert candidate["detail"] is None or isinstance(candidate["detail"], str)

    untrusted = next(
        candidate for candidate in candidates if candidate["path"].endswith('untrusted"entry.json')
    )
    assert untrusted["classification"] == "invalid"
    assert untrusted["run_id"] is None
    assert untrusted["path_run_id"] is None
    assert untrusted["archive_directory"] is None
    assert untrusted["registry"] is None

    invalid_payload = next(
        candidate
        for candidate in candidates
        if candidate["path_run_id"] == "00000000-0000-4000-8000-000000000006"
    )
    assert invalid_payload["run_id"] is None
    assert invalid_payload["archive_present"] is True
    assert {candidate["classification"] for candidate in candidates[-2:]} == {
        "completed",
        "incomplete",
    }


def test_registry_projection_has_explicit_optionals_and_decimal_integer_strings() -> None:
    _, document = _json_document()
    registries = [
        candidate["registry"]
        for candidate in document["candidates"]
        if candidate["registry"] is not None
    ]
    assert registries

    for registry in registries:
        assert set(registry) == REGISTRY_FIELDS
        assert registry["registry_schema_version"] == 1
        assert registry["server_protocol_version"] == 1
        assert UUID_RE.fullmatch(registry["run_id"])
        assert DECIMAL_RE.fullmatch(registry["owner_pid"])
        assert DECIMAL_RE.fullmatch(registry["port"])
        assert "advertise_url" in registry
        assert registry["advertise_url"] is None
        assert registry["security_scope"] == "trusted_network"


def test_human_fixture_keeps_full_identities_paths_and_all_candidate_states() -> None:
    human = (FIXTURES / "runs-human.txt").read_text(encoding="utf-8")
    _, document = _json_document()

    assert human.endswith("\n")
    assert "\x1b[" not in human
    assert "..." not in human
    assert f"candidate_count: {len(document['candidates'])}\n" in human
    assert human.count("\ncandidate ") == len(document["candidates"])

    for candidate in document["candidates"]:
        assert f"  classification: {candidate['classification']}\n" in human
        assert f"  path: {candidate['path']}\n" in human
        expected_run_id = candidate["run_id"] or "null"
        assert f"  run_id: {expected_run_id}\n" in human
