from __future__ import annotations

import ast
import copy
import hashlib
import importlib.util
import json
import shutil
import sys
from pathlib import Path

import pytest


ROOT = Path(__file__).resolve().parents[2]
VERIFIER_PATH = ROOT / "scripts" / "verify_diagnostic_fixtures.py"
FIXTURES = ROOT / "tests" / "fixtures" / "diagnostics" / "events"


def load_verifier():
    spec = importlib.util.spec_from_file_location("verify_diagnostic_fixtures", VERIFIER_PATH)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


def test_checked_in_event_fixtures_pass_independent_verification() -> None:
    verifier = load_verifier()

    summary = verifier.verify_event_fixtures(FIXTURES)

    assert summary.fixture_count == 18
    assert summary.valid_event_count >= 14
    assert summary.malformed_case_count >= 8
    assert summary.event_kinds == verifier.EVENT_KINDS
    assert summary.span_kinds == verifier.SPAN_KINDS
    assert summary.instant_kinds == verifier.INSTANT_KINDS
    assert summary.counter_kinds == verifier.COUNTER_KINDS


def test_reverse_loading_preserves_each_checked_in_event_byte_string() -> None:
    verifier = load_verifier()

    forward = verifier.canonical_event_bytes(FIXTURES, reverse=False)
    reverse = verifier.canonical_event_bytes(FIXTURES, reverse=True)

    assert forward == reverse


def test_decoder_independently_rejects_uuid_decimal_discriminant_and_optional_drift() -> None:
    verifier = load_verifier()
    event = json.loads((FIXTURES / "context-usage-sampled.json").read_text(encoding="utf-8"))[0]

    mutations = []
    uppercase_uuid = copy.deepcopy(event)
    uppercase_uuid["run_id"] = uppercase_uuid["run_id"].upper()
    mutations.append((uppercase_uuid, "uuid"))
    noncanonical_decimal = copy.deepcopy(event)
    noncanonical_decimal["cumulative_cost_amount"] = "1.2300"
    mutations.append((noncanonical_decimal, "decimal"))
    unknown_discriminant = copy.deepcopy(event)
    unknown_discriminant["kind"] = "future_event"
    mutations.append((unknown_discriminant, "discriminant"))
    missing_optional = copy.deepcopy(event)
    del missing_optional["observed_elapsed_ns"]
    mutations.append((missing_optional, "fields"))

    for malformed, code in mutations:
        with pytest.raises(verifier.FixtureValidationError, match=code):
            verifier.validate_event(malformed)


def test_arbitrary_token_integer_is_not_limited_to_u64() -> None:
    verifier = load_verifier()
    event = json.loads(
        (FIXTURES / "act-token-usage-finalized.json").read_text(encoding="utf-8")
    )[0]
    event["provider_total_tokens"] = "9" * 500

    verifier.validate_event(event)

    event["provider_total_tokens"] = "0" * 500
    with pytest.raises(verifier.FixtureValidationError, match="token_integer"):
        verifier.validate_event(event)


def test_manifest_sha_detects_fixture_tampering(tmp_path: Path) -> None:
    verifier = load_verifier()
    copied = tmp_path / "events"
    shutil.copytree(FIXTURES, copied)
    path = copied / "agent-message-delta.json"
    path.write_bytes(path.read_bytes().replace("Troupe".encode(), b"troupe", 1))
    assert hashlib.sha256(path.read_bytes()).hexdigest() != next(
        entry["sha256"]
        for entry in json.loads((copied / "manifest.json").read_text(encoding="utf-8"))["fixtures"]
        if entry["file"] == path.name
    )

    with pytest.raises(verifier.FixtureValidationError, match="sha256"):
        verifier.verify_event_fixtures(copied)


def test_verifier_uses_only_the_python_standard_library() -> None:
    source = VERIFIER_PATH.read_text(encoding="utf-8")
    imports = set()
    for node in ast.walk(ast.parse(source)):
        if isinstance(node, ast.Import):
            imports.update(alias.name.partition(".")[0] for alias in node.names)
        elif isinstance(node, ast.ImportFrom) and node.module is not None:
            imports.add(node.module.partition(".")[0])

    assert imports <= set(sys.stdlib_module_names) | {"__future__"}
