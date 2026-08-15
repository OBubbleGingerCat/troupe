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
VIEW_FIXTURES = ROOT / "tests" / "fixtures" / "diagnostics" / "views"


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


def test_checked_in_view_fixtures_pass_independent_verification() -> None:
    verifier = load_verifier()

    summary = verifier.verify_view_fixtures(VIEW_FIXTURES)

    assert summary.fixture_count == 8
    assert summary.renderers == {"timeline", "metric", "table", "time_series"}
    assert summary.invalid_case_count == 9
    assert summary.max_table_rows == 500
    assert summary.max_time_series_points == 1024


def test_reverse_loading_preserves_each_checked_in_view_byte_string() -> None:
    verifier = load_verifier()

    forward = verifier.canonical_view_bytes(VIEW_FIXTURES, reverse=False)
    reverse = verifier.canonical_view_bytes(VIEW_FIXTURES, reverse=True)

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


def test_view_manifest_sha_and_closed_descriptor_validation_detect_drift(tmp_path: Path) -> None:
    verifier = load_verifier()
    copied = tmp_path / "views"
    shutil.copytree(VIEW_FIXTURES, copied)
    path = copied / "timeline.json"
    payload = json.loads(path.read_text(encoding="utf-8"))
    payload["descriptor"]["query"]["sql"] = "select * from events"
    path.write_text(json.dumps(payload, separators=(",", ":")) + "\n", encoding="utf-8")

    with pytest.raises(verifier.FixtureValidationError, match="sha256"):
        verifier.verify_view_fixtures(copied)

    manifest = json.loads((copied / "manifest.json").read_text(encoding="utf-8"))
    for entry in manifest["fixtures"]:
        if entry["file"] == path.name:
            entry["sha256"] = hashlib.sha256(path.read_bytes()).hexdigest()
    (copied / "manifest.json").write_text(
        json.dumps(manifest, separators=(",", ":")) + "\n", encoding="utf-8"
    )
    with pytest.raises(verifier.FixtureValidationError, match="fields"):
        verifier.verify_view_fixtures(copied)


def test_view_decoder_enforces_group_caps_count_and_empty_series_contract() -> None:
    verifier = load_verifier()

    timeline = json.loads((VIEW_FIXTURES / "timeline.json").read_text(encoding="utf-8"))
    response = timeline["response"]
    response["binding"].update(
        captured_watermark="1", captured_elapsed_end_ns="20", range_end_ns="20"
    )
    response["coverage"].update(matched_count="1", contributing_count="1")
    response["rows"] = [
        {
            "sequence": "1",
            "group": {
                "dimension": {"dimension": "actor"},
                "value": {"type": "string", "value": "actor-1"},
            },
            "item_type": "span",
            "name": "cue.execution",
            "start_ns": "10",
            "end_ns": "20",
            "scope": {
                "scene_id": "scene-1",
                "actor_id": "actor-1",
                "cue_id": "cue-1",
                "effect_id": None,
                "act_id": None,
                "tool_call_id": None,
                "session_generation": None,
            },
            "outcome": "completed",
        }
    ]
    record = verifier.validate_view_record(timeline["descriptor"])
    verifier.validate_view_response(response, record)
    future_row = copy.deepcopy(timeline["response"])
    future_row["rows"][0]["sequence"] = "2"
    with pytest.raises(verifier.FixtureValidationError, match="sequence"):
        verifier.validate_view_response(future_row, record)
    outside_range = copy.deepcopy(timeline["response"])
    outside_range["rows"][0].update(start_ns="20", end_ns="20")
    with pytest.raises(verifier.FixtureValidationError, match="binding"):
        verifier.validate_view_response(outside_range, record)
    wrong_source = copy.deepcopy(timeline["response"])
    wrong_source["rows"][0].update(
        item_type="instant", name="cue.admitted", end_ns=None, outcome=None
    )
    with pytest.raises(verifier.FixtureValidationError, match="source"):
        verifier.validate_view_response(wrong_source, record)
    outside_selection = copy.deepcopy(timeline)
    outside_selection["descriptor"]["scope"] = "selection"
    outside_selection["response"]["binding"].update(
        scope="selection",
        selected_scope={
            "scene_id": "scene-1",
            "actor_id": "actor-2",
            "cue_id": None,
            "effect_id": None,
            "act_id": None,
            "tool_call_id": None,
            "session_generation": None,
        },
    )
    selected_record = verifier.validate_view_record(outside_selection["descriptor"])
    with pytest.raises(verifier.FixtureValidationError, match="binding"):
        verifier.validate_view_response(outside_selection["response"], selected_record)
    wrong_value = copy.deepcopy(timeline["response"])
    wrong_value["rows"][0]["group"]["value"]["value"] = "actor-2"
    with pytest.raises(verifier.FixtureValidationError, match="group"):
        verifier.validate_view_response(wrong_value, record)
    timeline["response"]["rows"][0]["group"]["dimension"] = {"dimension": "cue"}
    with pytest.raises(verifier.FixtureValidationError, match="group"):
        verifier.validate_view_response(timeline["response"], record)

    metric = json.loads((VIEW_FIXTURES / "metric.json").read_text(encoding="utf-8"))
    metric_record = verifier.validate_view_record(metric["descriptor"])
    boundary = copy.deepcopy(metric["response"])
    boundary["series"] = []
    for index in range(64):
        series = copy.deepcopy(metric["response"]["series"][0])
        series["group"]["value"]["value"] = f"act-{index}"
        boundary["series"].append(series)
    verifier.validate_view_response(boundary, metric_record)
    duplicate = copy.deepcopy(metric["response"])
    duplicate["series"] = [duplicate["series"][0], copy.deepcopy(duplicate["series"][0])]
    with pytest.raises(verifier.FixtureValidationError, match="group"):
        verifier.validate_view_response(duplicate, metric_record)
    invalid_group = copy.deepcopy(metric["response"])
    invalid_group["series"][0]["group"]["value"] = {"type": "null"}
    with pytest.raises(verifier.FixtureValidationError, match="group"):
        verifier.validate_view_response(invalid_group, metric_record)
    metric["response"]["series"] = [metric["response"]["series"][0]] * 65
    with pytest.raises(verifier.FixtureValidationError, match="series_cap"):
        verifier.validate_view_response(metric["response"], metric_record)

    count = json.loads((VIEW_FIXTURES / "metric.json").read_text(encoding="utf-8"))
    count["descriptor"]["query"]["reducer"] = "count"
    count["response"]["series"][0]["value"] = {
        "aggregate": "exact",
        "value": {"type": "decimal", "value": "1.5"},
    }
    count_record = verifier.validate_view_record(count["descriptor"])
    with pytest.raises(verifier.FixtureValidationError, match="reducer"):
        verifier.validate_view_response(count["response"], count_record)

    fractional_tokens = json.loads(
        (VIEW_FIXTURES / "metric.json").read_text(encoding="utf-8")
    )
    fractional_tokens["response"]["series"][0]["value"]["numerator"] = {
        "type": "decimal",
        "value": "1.5",
    }
    fractional_record = verifier.validate_view_record(fractional_tokens["descriptor"])
    with pytest.raises(verifier.FixtureValidationError, match="source"):
        verifier.validate_view_response(fractional_tokens["response"], fractional_record)

    timeseries = json.loads(
        (VIEW_FIXTURES / "timeseries.json").read_text(encoding="utf-8")
    )
    timeseries["response"]["series"] = []
    timeseries_record = verifier.validate_view_record(timeseries["descriptor"])
    verifier.validate_view_response(timeseries["response"], timeseries_record)

    unsafe = copy.deepcopy(timeseries["descriptor"])
    unsafe["query"]["group_by"]["key"] = "<script>"
    with pytest.raises(verifier.FixtureValidationError, match="custom_key"):
        verifier.validate_view_record(unsafe)

    table = json.loads((VIEW_FIXTURES / "table.json").read_text(encoding="utf-8"))
    table["response"]["rows"][1]["sequence"] = "1"
    table_record = verifier.validate_view_record(table["descriptor"])
    with pytest.raises(verifier.FixtureValidationError, match="sequence"):
        verifier.validate_view_response(table["response"], table_record)


def test_verifier_uses_only_the_python_standard_library() -> None:
    source = VERIFIER_PATH.read_text(encoding="utf-8")
    imports = set()
    for node in ast.walk(ast.parse(source)):
        if isinstance(node, ast.Import):
            imports.update(alias.name.partition(".")[0] for alias in node.names)
        elif isinstance(node, ast.ImportFrom) and node.module is not None:
            imports.add(node.module.partition(".")[0])

    assert imports <= set(sys.stdlib_module_names) | {"__future__"}
