#!/usr/bin/env python3
from __future__ import annotations

import argparse
import hashlib
import json
import re
import sys
import unicodedata
import uuid
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Callable, Final


MAX_U64: Final = 18_446_744_073_709_551_615
COMMON_FIELDS: Final = frozenset(
    {"schema_version", "run_id", "sequence", "elapsed_ns", "scope", "caused_by", "kind"}
)
SCOPE_FIELDS: Final = frozenset(
    {
        "scene_id",
        "actor_id",
        "cue_id",
        "effect_id",
        "act_id",
        "tool_call_id",
        "session_generation",
    }
)
EVENT_KINDS: Final = frozenset(
    {
        "span_started",
        "span_finished",
        "instant_occurred",
        "counter_sampled",
        "agent_message_delta",
        "agent_message_completed",
        "agent_plan_snapshot",
        "context_usage_sampled",
        "act_token_usage_finalized",
        "observation_gap",
        "custom_span_started",
        "custom_span_finished",
        "custom_instant_occurred",
        "custom_counter_sampled",
    }
)
SPAN_KINDS: Final = frozenset(
    {
        "run.lifecycle",
        "production.path_resolution",
        "production.load",
        "production.construct",
        "production.start",
        "production.stop",
        "production.shutdown",
        "scene.lifecycle",
        "scene.drain",
        "scene.cleanup",
        "actor.handle_lifetime",
        "cue.mailbox_wait",
        "cue.execution",
        "effect.lifecycle",
        "agent.session.opening",
        "agent.session.lifecycle",
        "agent.session.closing",
        "act.lifecycle",
        "act.caller",
        "agent.turn",
        "agent.thinking",
        "tool.call",
    }
)
INSTANT_KINDS: Final = frozenset(
    {
        "actor.cast",
        "cue.admitted",
        "cue.enqueued",
        "cue.dispatched",
        "cue.cancel_requested",
        "effect.created",
        "effect.returned",
        "effect.consumed",
        "agent.session.ready",
        "agent.session.broken",
        "act.admitted",
        "act.waiting_ready",
        "act.prompt_submitted",
        "act.cancel_requested",
        "act.supervisor_handoff",
        "agent.turn.activity",
        "agent.turn.terminal",
        "agent.turn.settled",
        "tool.updated",
        "result.submitted",
        "result.rejected",
        "result.repair_requested",
        "result.accepted",
        "result.missing",
        "diagnostic.component_failed",
    }
)
COUNTER_KINDS: Final = frozenset(
    {
        "actor.mailbox_depth",
        "cue.active",
        "agent.turn.active",
        "result.validation_rejections",
        "diagnostic.dropped_events",
    }
)
CAUSAL_RELATIONS: Final = frozenset(
    {"dispatch", "return", "handoff", "retry", "follows_from"}
)
SPAN_OUTCOMES: Final = frozenset({"completed", "cancelled", "failed"})
EVENT_FIELDS: Final = {
    "span_started": frozenset({"span_kind", "detail", "parent_span_id"}),
    "span_finished": frozenset({"span_id", "outcome", "error_code"}),
    "instant_occurred": frozenset({"instant_kind", "detail", "containing_span_id"}),
    "counter_sampled": frozenset({"counter_kind", "value"}),
    "agent_message_delta": frozenset({"message_id", "source_message_id", "text_delta"}),
    "agent_message_completed": frozenset(
        {"message_id", "utf8_bytes", "unicode_scalar_count", "truncated"}
    ),
    "agent_plan_snapshot": frozenset({"entries", "truncated"}),
    "context_usage_sampled": frozenset(
        {
            "context_used_tokens",
            "context_window_tokens",
            "cumulative_cost_amount",
            "cumulative_cost_currency",
            "sample_origin",
            "observed_elapsed_ns",
        }
    ),
    "act_token_usage_finalized": frozenset(
        {
            "availability",
            "source",
            "unavailable_reason",
            "provider_total_tokens",
            "input_tokens",
            "output_tokens",
            "thought_tokens",
            "cached_read_tokens",
            "cached_write_tokens",
        }
    ),
    "observation_gap": frozenset(
        {
            "producer",
            "component",
            "reason",
            "dropped_count",
            "affected_elapsed",
            "affected_kind",
            "affected_scope",
        }
    ),
    "custom_span_started": frozenset({"name", "parent_span_id", "attributes"}),
    "custom_span_finished": frozenset({"span_id", "outcome"}),
    "custom_instant_occurred": frozenset(
        {"name", "containing_span_id", "severity", "attributes"}
    ),
    "custom_counter_sampled": frozenset({"name", "value", "unit", "dimensions"}),
}
EXPECTED_FILES: Final = (
    "act-token-usage-finalized.json",
    "agent-message-completed.json",
    "agent-message-delta.json",
    "agent-plan-snapshot.json",
    "context-usage-sampled.json",
    "counter-sampled.json",
    "custom-counter-sampled.json",
    "custom-instant-occurred.json",
    "custom-span-finished.json",
    "custom-span-started.json",
    "diagnostic-component-failed.json",
    "instant-occurred.json",
    "limits.json",
    "malformed.json",
    "nested-overlap.json",
    "observation-gap.json",
    "span-finished.json",
    "span-started.json",
)
EXPECTED_VIEW_FILES: Final = (
    "compatible.json",
    "corrupt.json",
    "invalid-descriptor.json",
    "metric.json",
    "newer.json",
    "table.json",
    "timeline.json",
    "timeseries.json",
)
VIEW_FIXTURE_FORMATS: Final = {
    "compatible.json": "compatible",
    "corrupt.json": "archived_record",
    "invalid-descriptor.json": "invalid_descriptors",
    "metric.json": "renderer_fixture",
    "newer.json": "archived_record",
    "table.json": "renderer_fixture",
    "timeline.json": "renderer_fixture",
    "timeseries.json": "renderer_fixture",
}
VIEW_RENDERERS: Final = frozenset({"timeline", "metric", "table", "time_series"})
VIEW_TIME_RANGES: Final = frozenset({"viewport", "run"})
VIEW_SCOPES: Final = frozenset({"selection", "run"})
VIEW_REDUCERS: Final = frozenset({"count", "sum", "min", "max", "mean", "latest"})
TOKEN_METRICS: Final = frozenset(
    {
        "provider_total_tokens",
        "input_tokens",
        "output_tokens",
        "thought_tokens",
        "cached_read_tokens",
        "cached_write_tokens",
    }
)
MAX_PAGE_ROWS: Final = 500
MAX_METRIC_SERIES: Final = 64
MAX_TIME_SERIES_POINTS: Final = 1024
MAX_TIME_SERIES_SERIES: Final = 64
VIEW_ID: Final = re.compile(r"[a-z][a-z0-9_]*\Z")
HEX_SHA256: Final = re.compile(r"[0-9a-f]{64}\Z")
NONNEGATIVE_INTEGER: Final = re.compile(r"(?:0|[1-9][0-9]*)\Z")
CANONICAL_INTEGER: Final = re.compile(r"(?:0|-?[1-9][0-9]*)\Z")
CANONICAL_DECIMAL: Final = re.compile(
    r"-?(?:0|[1-9][0-9]*)(?:\.[0-9]*[1-9])?\Z"
)
IDENTIFIER_SEGMENT: Final = re.compile(r"[a-z][a-z0-9_]*\Z")
RESERVED_CUSTOM_ROOT: Final = "troupe"


class FixtureValidationError(RuntimeError):
    def __init__(self, code: str, path: str, detail: str) -> None:
        self.code = code
        self.path = path
        super().__init__(f"{code} at {path}: {detail}")


@dataclass(frozen=True, slots=True)
class VerificationSummary:
    fixture_count: int
    valid_event_count: int
    malformed_case_count: int
    event_kinds: frozenset[str]
    span_kinds: frozenset[str]
    instant_kinds: frozenset[str]
    counter_kinds: frozenset[str]


@dataclass(frozen=True, slots=True)
class ViewVerificationSummary:
    fixture_count: int
    renderers: frozenset[str]
    invalid_case_count: int
    max_table_rows: int
    max_time_series_points: int


def _fail(code: str, path: str, detail: str) -> None:
    raise FixtureValidationError(code, path, detail)


def _object(value: Any, path: str) -> dict[str, Any]:
    if not isinstance(value, dict):
        _fail("type", path, "expected object")
    return value


def _array(value: Any, path: str) -> list[Any]:
    if not isinstance(value, list):
        _fail("type", path, "expected array")
    return value


def _closed(value: dict[str, Any], fields: frozenset[str], path: str) -> None:
    actual = frozenset(value)
    if actual != fields:
        _fail(
            "fields",
            path,
            f"missing={sorted(fields - actual)}, extra={sorted(actual - fields)}",
        )


def _string(value: Any, path: str) -> str:
    if not isinstance(value, str):
        _fail("type", path, "expected string")
    return value


def _boolean(value: Any, path: str) -> bool:
    if type(value) is not bool:
        _fail("type", path, "expected boolean")
    return value


def _enum(value: Any, allowed: frozenset[str], path: str, code: str = "discriminant") -> str:
    text = _string(value, path)
    if text not in allowed:
        _fail(code, path, f"unknown value {text!r}")
    return text


def _optional(value: Any, validator: Callable[[Any, str], Any], path: str) -> Any:
    return None if value is None else validator(value, path)


def _u64(value: Any, path: str) -> int:
    text = _string(value, path)
    if NONNEGATIVE_INTEGER.fullmatch(text) is None:
        _fail("u64", path, "not a canonical nonnegative integer string")
    parsed = int(text)
    if parsed > MAX_U64:
        _fail("u64", path, "value exceeds u64")
    return parsed


def _token_integer(value: Any, path: str) -> int:
    text = _string(value, path)
    if NONNEGATIVE_INTEGER.fullmatch(text) is None:
        _fail("token_integer", path, "not a canonical nonnegative integer string")
    return int(text)


def _canonical_integer(value: Any, path: str) -> str:
    text = _string(value, path)
    if CANONICAL_INTEGER.fullmatch(text) is None:
        _fail("integer", path, "not a canonical integer string")
    return text


def _decimal(value: Any, path: str) -> str:
    text = _string(value, path)
    if CANONICAL_DECIMAL.fullmatch(text) is None or text == "-0":
        _fail("decimal", path, "not a normalized fixed decimal string")
    return text


def _currency(value: Any, path: str) -> str:
    text = _string(value, path)
    if len(text) != 3 or not text.isascii() or not text.isupper() or not text.isalpha():
        _fail("currency", path, "expected three uppercase ASCII letters")
    return text


def _run_local_id(value: Any, path: str) -> str:
    text = _string(value, path)
    if not text or len(text.encode("utf-8")) > 128 or not text.isascii():
        _fail("run_local_id", path, "expected nonempty ASCII with at most 128 bytes")
    return text


def _uuid(value: Any, path: str) -> str:
    text = _string(value, path)
    try:
        parsed = uuid.UUID(text)
    except (ValueError, AttributeError) as error:
        raise FixtureValidationError("uuid", path, "invalid UUID") from error
    if str(parsed) != text:
        _fail("uuid", path, "expected lowercase canonical hyphenated form")
    return text


def _scope(value: Any, path: str) -> dict[str, Any]:
    scope = _object(value, path)
    _closed(scope, SCOPE_FIELDS, path)
    for field in (
        "scene_id",
        "actor_id",
        "cue_id",
        "effect_id",
        "act_id",
        "tool_call_id",
    ):
        _optional(scope[field], _run_local_id, f"{path}.{field}")
    generation = _optional(scope["session_generation"], _u64, f"{path}.session_generation")
    if generation == 0:
        _fail("scope", f"{path}.session_generation", "zero is the unknown sentinel")
    return scope


def _causal_links(value: Any, sequence: int, path: str) -> list[dict[str, Any]]:
    links = _array(value, path)
    if len(links) > 16:
        _fail("causal", path, "more than 16 links")
    for index, raw in enumerate(links):
        item_path = f"{path}[{index}]"
        link = _object(raw, item_path)
        _closed(link, frozenset({"source_sequence", "relation"}), item_path)
        source = _u64(link["source_sequence"], f"{item_path}.source_sequence")
        if source >= sequence:
            _fail("causal", item_path, "link is not backward")
        _enum(link["relation"], CAUSAL_RELATIONS, f"{item_path}.relation")
    return links


def _empty_detail(value: Any, path: str) -> None:
    _closed(_object(value, path), frozenset(), path)


def _optional_string(value: Any, path: str) -> Any:
    return _optional(value, _string, path)


def _actor_detail(value: Any, path: str) -> None:
    detail = _object(value, path)
    _closed(detail, frozenset({"display_name", "actor_type"}), path)
    _string(detail["display_name"], f"{path}.display_name")
    _string(detail["actor_type"], f"{path}.actor_type")


def _effect_detail(value: Any, path: str) -> None:
    detail = _object(value, path)
    _closed(detail, frozenset({"effect_type"}), path)
    _string(detail["effect_type"], f"{path}.effect_type")


def _session_detail(value: Any, path: str) -> None:
    detail = _object(value, path)
    _closed(detail, frozenset({"provider", "effective_model", "effective_effort"}), path)
    _string(detail["provider"], f"{path}.provider")
    _optional_string(detail["effective_model"], f"{path}.effective_model")
    _optional_string(detail["effective_effort"], f"{path}.effective_effort")


def _tool_detail(value: Any, path: str) -> None:
    detail = _object(value, path)
    _closed(detail, frozenset({"title", "tool_kind", "status", "error_code"}), path)
    _string(detail["title"], f"{path}.title")
    _enum(
        detail["tool_kind"],
        frozenset(
            {"read", "edit", "delete", "move", "search", "execute", "think", "fetch", "switch_mode", "other"}
        ),
        f"{path}.tool_kind",
    )
    _enum(
        detail["status"],
        frozenset({"pending", "in_progress", "completed", "failed"}),
        f"{path}.status",
    )
    _optional_string(detail["error_code"], f"{path}.error_code")


def _result_detail(value: Any, path: str) -> None:
    detail = _object(value, path)
    _closed(detail, frozenset({"issue", "error_code"}), path)
    if detail["issue"] is not None:
        issue = _object(detail["issue"], f"{path}.issue")
        _closed(issue, frozenset({"code", "path"}), f"{path}.issue")
        _string(issue["code"], f"{path}.issue.code")
        _string(issue["path"], f"{path}.issue.path")
    _optional_string(detail["error_code"], f"{path}.error_code")


def _component_failure_detail(value: Any, path: str) -> tuple[str, str]:
    detail = _object(value, path)
    _closed(
        detail,
        frozenset({"component", "component_id", "stage", "error_code", "related_event_sequence"}),
        path,
    )
    if detail["component"] != "sink":
        _fail("component_failure", f"{path}.component", "component must be sink")
    _run_local_id(detail["component_id"], f"{path}.component_id")
    stage = _enum(detail["stage"], frozenset({"enqueue", "callback"}), f"{path}.stage")
    error_code = _enum(
        detail["error_code"],
        frozenset({"delivery_queue_unavailable", "callback_raised", "callback_invalid_return"}),
        f"{path}.error_code",
    )
    allowed = {
        ("enqueue", "delivery_queue_unavailable"),
        ("callback", "callback_raised"),
        ("callback", "callback_invalid_return"),
    }
    if (stage, error_code) not in allowed:
        _fail("component_failure", path, "stage and error code do not match")
    _optional(detail["related_event_sequence"], _u64, f"{path}.related_event_sequence")
    return stage, error_code


def _span_detail(kind: str, value: Any, path: str) -> None:
    if kind in {
        "run.lifecycle",
        "production.start",
        "production.stop",
        "production.shutdown",
        "scene.lifecycle",
        "scene.drain",
        "scene.cleanup",
        "cue.mailbox_wait",
        "cue.execution",
        "act.caller",
        "agent.thinking",
    }:
        _empty_detail(value, path)
    elif kind == "production.path_resolution":
        detail = _object(value, path)
        _closed(detail, frozenset({"production_root", "package"}), path)
        _string(detail["production_root"], f"{path}.production_root")
        _string(detail["package"], f"{path}.package")
    elif kind == "production.load":
        detail = _object(value, path)
        _closed(detail, frozenset({"package"}), path)
        _string(detail["package"], f"{path}.package")
    elif kind == "production.construct":
        detail = _object(value, path)
        _closed(detail, frozenset({"package", "class_name"}), path)
        _string(detail["package"], f"{path}.package")
        _string(detail["class_name"], f"{path}.class_name")
    elif kind == "actor.handle_lifetime":
        _actor_detail(value, path)
    elif kind == "effect.lifecycle":
        _effect_detail(value, path)
    elif kind in {
        "agent.session.opening",
        "agent.session.lifecycle",
        "agent.session.closing",
        "act.lifecycle",
        "agent.turn",
    }:
        _session_detail(value, path)
    elif kind == "tool.call":
        _tool_detail(value, path)
    else:
        _fail("discriminant", path, f"unhandled span kind {kind!r}")


def _instant_detail(kind: str, value: Any, path: str) -> tuple[str, str] | None:
    if kind == "actor.cast":
        _actor_detail(value, path)
    elif kind in {
        "cue.admitted",
        "cue.enqueued",
        "cue.dispatched",
        "cue.cancel_requested",
        "act.admitted",
        "act.waiting_ready",
        "act.prompt_submitted",
        "act.cancel_requested",
        "act.supervisor_handoff",
        "agent.turn.activity",
    }:
        _empty_detail(value, path)
    elif kind in {"effect.created", "effect.returned", "effect.consumed"}:
        _effect_detail(value, path)
    elif kind == "agent.session.ready":
        _session_detail(value, path)
    elif kind == "agent.session.broken":
        detail = _object(value, path)
        _closed(
            detail,
            frozenset({"provider", "effective_model", "effective_effort", "error_code"}),
            path,
        )
        _string(detail["provider"], f"{path}.provider")
        _optional_string(detail["effective_model"], f"{path}.effective_model")
        _optional_string(detail["effective_effort"], f"{path}.effective_effort")
        _string(detail["error_code"], f"{path}.error_code")
    elif kind in {"agent.turn.terminal", "agent.turn.settled"}:
        detail = _object(value, path)
        _closed(detail, frozenset({"error_code"}), path)
        _optional_string(detail["error_code"], f"{path}.error_code")
    elif kind == "tool.updated":
        _tool_detail(value, path)
    elif kind in {
        "result.submitted",
        "result.rejected",
        "result.repair_requested",
        "result.accepted",
        "result.missing",
    }:
        _result_detail(value, path)
    elif kind == "diagnostic.component_failed":
        return _component_failure_detail(value, path)
    else:
        _fail("discriminant", path, f"unhandled instant kind {kind!r}")
    return None


def _custom_name(value: Any, path: str) -> str:
    name = _string(value, path)
    if not name.isascii() or not name or len(name.encode("utf-8")) > 128:
        _fail("custom_name", path, "name is out of bounds")
    segments = name.split(".")
    if (
        len(segments) < 2
        or segments[0] == RESERVED_CUSTOM_ROOT
        or any(IDENTIFIER_SEGMENT.fullmatch(segment) is None for segment in segments)
    ):
        _fail("custom_name", path, "name is invalid or reserved")
    return name


def _custom_key(value: Any, path: str) -> str:
    key = _string(value, path)
    if not key or len(key.encode("utf-8")) > 64:
        _fail("custom_key", path, "key is out of bounds")
    return key


def _tagged_scalar(value: Any, path: str, *, attribute: bool, dimension: bool = False) -> None:
    tagged = _object(value, path)
    scalar_types = {"null", "boolean", "integer", "decimal", "string"}
    if attribute:
        scalar_types.add("list")
    if dimension:
        scalar_types.remove("null")
    kind = _string(tagged.get("type"), f"{path}.type")
    if kind not in scalar_types:
        _fail("discriminant", f"{path}.type", f"unknown scalar type {kind!r}")
    fields = frozenset({"type"}) if kind == "null" else frozenset({"type", "value"})
    _closed(tagged, fields, path)
    if kind == "null":
        return
    scalar = tagged["value"]
    if kind == "boolean":
        _boolean(scalar, f"{path}.value")
    elif kind == "integer":
        _canonical_integer(scalar, f"{path}.value")
    elif kind == "decimal":
        _decimal(scalar, f"{path}.value")
    elif kind == "string":
        _string(scalar, f"{path}.value")
    elif kind == "list":
        values = _array(scalar, f"{path}.value")
        if len(values) > 64:
            _fail("custom_list", f"{path}.value", "list is too long")
        for index, item in enumerate(values):
            _tagged_scalar(item, f"{path}.value[{index}]", attribute=False)


def _attributes(value: Any, path: str) -> None:
    attributes = _object(value, path)
    if len(attributes) > 32:
        _fail("custom_attributes", path, "too many attributes")
    if list(attributes) != sorted(attributes):
        _fail("custom_order", path, "attribute keys are not canonical")
    for key, item in attributes.items():
        _custom_key(key, f"{path}.<key>")
        _tagged_scalar(item, f"{path}.{key}", attribute=True)


def _dimensions(value: Any, path: str) -> None:
    dimensions = _object(value, path)
    if len(dimensions) > 8:
        _fail("custom_dimensions", path, "too many dimensions")
    if list(dimensions) != sorted(dimensions):
        _fail("custom_order", path, "dimension keys are not canonical")
    for key, item in dimensions.items():
        _custom_key(key, f"{path}.<key>")
        _tagged_scalar(item, f"{path}.{key}", attribute=False, dimension=True)


def _custom_number(value: Any, path: str) -> None:
    tagged = _object(value, path)
    _closed(tagged, frozenset({"type", "value"}), path)
    kind = _enum(tagged["type"], frozenset({"integer", "decimal"}), f"{path}.type")
    if kind == "integer":
        _canonical_integer(tagged["value"], f"{path}.value")
    else:
        _decimal(tagged["value"], f"{path}.value")


def validate_event(value: Any, path: str = "event") -> None:
    event = _object(value, path)
    kind = _enum(event.get("kind"), EVENT_KINDS, f"{path}.kind")
    _closed(event, COMMON_FIELDS | EVENT_FIELDS[kind], path)
    if type(event["schema_version"]) is not int or event["schema_version"] != 1:
        _fail("schema_version", f"{path}.schema_version", "expected integer 1")
    _uuid(event["run_id"], f"{path}.run_id")
    sequence = _u64(event["sequence"], f"{path}.sequence")
    if sequence == 0:
        _fail("u64", f"{path}.sequence", "sequence must start at one")
    _u64(event["elapsed_ns"], f"{path}.elapsed_ns")
    _scope(event["scope"], f"{path}.scope")
    _causal_links(event["caused_by"], sequence, f"{path}.caused_by")

    if kind == "span_started":
        span_kind = _enum(event["span_kind"], SPAN_KINDS, f"{path}.span_kind")
        _span_detail(span_kind, event["detail"], f"{path}.detail")
        _optional(event["parent_span_id"], _u64, f"{path}.parent_span_id")
    elif kind == "span_finished":
        _u64(event["span_id"], f"{path}.span_id")
        _enum(event["outcome"], SPAN_OUTCOMES, f"{path}.outcome")
        _optional_string(event["error_code"], f"{path}.error_code")
    elif kind == "instant_occurred":
        instant_kind = _enum(event["instant_kind"], INSTANT_KINDS, f"{path}.instant_kind")
        _instant_detail(instant_kind, event["detail"], f"{path}.detail")
        _optional(event["containing_span_id"], _u64, f"{path}.containing_span_id")
    elif kind == "counter_sampled":
        _enum(event["counter_kind"], COUNTER_KINDS, f"{path}.counter_kind")
        _u64(event["value"], f"{path}.value")
    elif kind == "agent_message_delta":
        _run_local_id(event["message_id"], f"{path}.message_id")
        _optional_string(event["source_message_id"], f"{path}.source_message_id")
        _string(event["text_delta"], f"{path}.text_delta")
    elif kind == "agent_message_completed":
        _run_local_id(event["message_id"], f"{path}.message_id")
        _u64(event["utf8_bytes"], f"{path}.utf8_bytes")
        _u64(event["unicode_scalar_count"], f"{path}.unicode_scalar_count")
        _boolean(event["truncated"], f"{path}.truncated")
    elif kind == "agent_plan_snapshot":
        for index, raw in enumerate(_array(event["entries"], f"{path}.entries")):
            entry_path = f"{path}.entries[{index}]"
            entry = _object(raw, entry_path)
            _closed(entry, frozenset({"content", "priority", "status"}), entry_path)
            _string(entry["content"], f"{entry_path}.content")
            _enum(entry["priority"], frozenset({"high", "medium", "low"}), f"{entry_path}.priority")
            _enum(
                entry["status"],
                frozenset({"pending", "in_progress", "completed"}),
                f"{entry_path}.status",
            )
        _boolean(event["truncated"], f"{path}.truncated")
    elif kind == "context_usage_sampled":
        used = _optional(event["context_used_tokens"], _u64, f"{path}.context_used_tokens")
        window = _optional(event["context_window_tokens"], _u64, f"{path}.context_window_tokens")
        amount = _optional(event["cumulative_cost_amount"], _decimal, f"{path}.cumulative_cost_amount")
        currency = _optional(event["cumulative_cost_currency"], _currency, f"{path}.cumulative_cost_currency")
        origin = _enum(
            event["sample_origin"],
            frozenset({"provider", "carried_forward"}),
            f"{path}.sample_origin",
        )
        observed = _optional(event["observed_elapsed_ns"], _u64, f"{path}.observed_elapsed_ns")
        if (amount is None) != (currency is None):
            _fail("optional", path, "cost amount and currency must appear together")
        if amount is not None and amount.startswith("-"):
            _fail("decimal", f"{path}.cumulative_cost_amount", "cost must be nonnegative")
        if used is not None and window is not None and used > window:
            _fail("context_usage", path, "used tokens exceed window")
        if origin == "carried_forward" and observed is None:
            _fail("optional", f"{path}.observed_elapsed_ns", "carried sample needs observation time")
    elif kind == "act_token_usage_finalized":
        availability = _enum(
            event["availability"],
            frozenset({"available", "partial", "unavailable"}),
            f"{path}.availability",
        )
        source = event["source"]
        if source is not None:
            _enum(source, frozenset({"acp.prompt_response.usage"}), f"{path}.source")
        reason = event["unavailable_reason"]
        if reason is not None:
            _enum(
                reason,
                frozenset(
                    {"prompt_not_submitted", "source_unsupported", "usage_not_reported", "turn_settlement_unknown"}
                ),
                f"{path}.unavailable_reason",
            )
        names = (
            "provider_total_tokens",
            "input_tokens",
            "output_tokens",
            "thought_tokens",
            "cached_read_tokens",
            "cached_write_tokens",
        )
        values = {
            name: _optional(event[name], _token_integer, f"{path}.{name}") for name in names
        }
        primary_complete = all(values[name] is not None for name in names[:3])
        any_value = any(value is not None for value in values.values())
        valid = (
            availability == "available" and primary_complete and source is not None and reason is None
        ) or (
            availability == "partial" and any_value and not primary_complete and source is not None and reason is None
        ) or (
            availability == "unavailable" and not any_value and source is None and reason is not None
        )
        if not valid:
            _fail("usage", path, "availability fields are inconsistent")
    elif kind == "observation_gap":
        _string(event["producer"], f"{path}.producer")
        _optional_string(event["component"], f"{path}.component")
        _string(event["reason"], f"{path}.reason")
        _optional(event["dropped_count"], _u64, f"{path}.dropped_count")
        if event["affected_elapsed"] is not None:
            interval = _object(event["affected_elapsed"], f"{path}.affected_elapsed")
            _closed(interval, frozenset({"start_ns", "end_ns"}), f"{path}.affected_elapsed")
            _u64(interval["start_ns"], f"{path}.affected_elapsed.start_ns")
            _u64(interval["end_ns"], f"{path}.affected_elapsed.end_ns")
        if event["affected_kind"] is not None:
            _enum(event["affected_kind"], EVENT_KINDS, f"{path}.affected_kind")
        if event["affected_scope"] is not None:
            _scope(event["affected_scope"], f"{path}.affected_scope")
    elif kind == "custom_span_started":
        _custom_name(event["name"], f"{path}.name")
        _optional(event["parent_span_id"], _u64, f"{path}.parent_span_id")
        _attributes(event["attributes"], f"{path}.attributes")
    elif kind == "custom_span_finished":
        _u64(event["span_id"], f"{path}.span_id")
        _enum(event["outcome"], SPAN_OUTCOMES, f"{path}.outcome")
    elif kind == "custom_instant_occurred":
        _custom_name(event["name"], f"{path}.name")
        _optional(event["containing_span_id"], _u64, f"{path}.containing_span_id")
        if event["severity"] is not None:
            _enum(
                event["severity"],
                frozenset({"debug", "info", "warning", "error"}),
                f"{path}.severity",
            )
        _attributes(event["attributes"], f"{path}.attributes")
    elif kind == "custom_counter_sampled":
        _custom_name(event["name"], f"{path}.name")
        _custom_number(event["value"], f"{path}.value")
        if event["unit"] is not None:
            unit = _string(event["unit"], f"{path}.unit")
            if not unit or len(unit.encode("utf-8")) > 32:
                _fail("custom_unit", f"{path}.unit", "unit is out of bounds")
        _dimensions(event["dimensions"], f"{path}.dimensions")


def _scope_contains(parent: dict[str, Any], child: dict[str, Any]) -> bool:
    for field in SCOPE_FIELDS:
        if parent[field] is not None and parent[field] != child[field]:
            return False
    return True


def validate_stream(events: list[Any], path: str = "events") -> None:
    run_id: str | None = None
    seen: set[int] = set()
    spans: dict[int, dict[str, Any]] = {}
    for index, raw in enumerate(events):
        event_path = f"{path}[{index}]"
        validate_event(raw, event_path)
        event = _object(raw, event_path)
        sequence = int(event["sequence"])
        elapsed = int(event["elapsed_ns"])
        if run_id is not None and event["run_id"] != run_id:
            _fail("reference", event_path, "cross-run stream")
        if sequence in seen:
            _fail("reference", event_path, "duplicate sequence")
        for link in event["caused_by"]:
            if int(link["source_sequence"]) not in seen:
                _fail("reference", event_path, "causal source has not appeared")

        kind = event["kind"]
        if kind in {"span_started", "custom_span_started"}:
            parent_id = event["parent_span_id"]
            if parent_id is not None:
                parent = spans.get(int(parent_id))
                if parent is None or parent["finished"] is not None:
                    _fail("reference", event_path, "parent span is missing or closed")
                if not _scope_contains(parent["scope"], event["scope"]) or elapsed < parent["started"]:
                    _fail("reference", event_path, "child is outside parent")
            spans[sequence] = {
                "family": "custom" if kind.startswith("custom") else "built_in",
                "scope": event["scope"],
                "started": elapsed,
                "finished": None,
                "latest_contained": None,
                "parent": None if parent_id is None else int(parent_id),
            }
        elif kind in {"span_finished", "custom_span_finished"}:
            span_id = int(event["span_id"])
            span = spans.get(span_id)
            expected_family = "custom" if kind.startswith("custom") else "built_in"
            if span is None or span["family"] != expected_family or span["finished"] is not None:
                _fail("reference", event_path, "span finish target is invalid")
            if span_id >= sequence or span["scope"] != event["scope"] or elapsed < span["started"]:
                _fail("reference", event_path, "span finish is outside its start")
            if span["latest_contained"] is not None and span["latest_contained"] > elapsed:
                _fail("reference", event_path, "contained event extends beyond finish")
            for child in spans.values():
                if child["parent"] == span_id and (child["finished"] is None or child["finished"] > elapsed):
                    _fail("reference", event_path, "parent finished before child")
            span["finished"] = elapsed
        elif kind in {"instant_occurred", "custom_instant_occurred"}:
            containing_id = event["containing_span_id"]
            if containing_id is not None:
                span = spans.get(int(containing_id))
                if span is None or span["finished"] is not None:
                    _fail("reference", event_path, "containing span is missing or closed")
                if not _scope_contains(span["scope"], event["scope"]) or elapsed < span["started"]:
                    _fail("reference", event_path, "instant is outside containing span")
                previous = span["latest_contained"]
                span["latest_contained"] = elapsed if previous is None else max(previous, elapsed)

        seen.add(sequence)
        run_id = event["run_id"]


def _duplicate_checked_object(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
    value: dict[str, Any] = {}
    for key, item in pairs:
        if key in value:
            _fail("json", key, "duplicate object field")
        value[key] = item
    return value


def _canonical_json(path: Path) -> Any:
    try:
        raw = path.read_bytes()
        text = raw.decode("utf-8")
        value = json.loads(text, object_pairs_hook=_duplicate_checked_object)
    except FixtureValidationError:
        raise
    except (OSError, UnicodeError, json.JSONDecodeError) as error:
        raise FixtureValidationError("json", str(path), str(error)) from error
    canonical = (json.dumps(value, ensure_ascii=False, separators=(",", ":")) + "\n").encode()
    if raw != canonical:
        _fail("canonical_json", str(path), "bytes are not canonical compact UTF-8 JSON plus LF")
    return value


def _load_manifest(root: Path) -> tuple[dict[str, Any], list[dict[str, Any]]]:
    manifest = _object(_canonical_json(root / "manifest.json"), "manifest")
    _closed(manifest, frozenset({"schema_version", "fixtures"}), "manifest")
    if type(manifest["schema_version"]) is not int or manifest["schema_version"] != 1:
        _fail("schema_version", "manifest.schema_version", "expected integer 1")
    entries = _array(manifest["fixtures"], "manifest.fixtures")
    files: list[str] = []
    for index, raw in enumerate(entries):
        path = f"manifest.fixtures[{index}]"
        entry = _object(raw, path)
        _closed(entry, frozenset({"file", "format", "sha256"}), path)
        name = _string(entry["file"], f"{path}.file")
        if Path(name).name != name or name == "manifest.json":
            _fail("manifest", f"{path}.file", "fixture name is not canonical")
        expected_format = "malformed_cases" if name == "malformed.json" else "event_array"
        if entry["format"] != expected_format:
            _fail("manifest", f"{path}.format", f"expected {expected_format}")
        digest = _string(entry["sha256"], f"{path}.sha256")
        if HEX_SHA256.fullmatch(digest) is None:
            _fail("sha256", f"{path}.sha256", "digest is not lowercase SHA-256")
        fixture_path = root / name
        try:
            actual = hashlib.sha256(fixture_path.read_bytes()).hexdigest()
        except OSError as error:
            raise FixtureValidationError("manifest", str(fixture_path), str(error)) from error
        if actual != digest:
            _fail("sha256", str(fixture_path), f"expected {digest}, got {actual}")
        files.append(name)
    if tuple(files) != EXPECTED_FILES:
        _fail("manifest", "manifest.fixtures", "fixture membership or order is not exact")
    return manifest, entries


def _walk(value: Any):
    yield value
    if isinstance(value, list):
        for item in value:
            yield from _walk(item)
    elif isinstance(value, dict):
        for item in value.values():
            yield from _walk(item)


def _assert_required_coverage(files: dict[str, list[dict[str, Any]]]) -> None:
    events = [event for values in files.values() for event in values]
    event_kinds = frozenset(event["kind"] for event in events)
    span_kinds = frozenset(
        event["span_kind"] for event in events if event["kind"] == "span_started"
    )
    instant_kinds = frozenset(
        event["instant_kind"] for event in events if event["kind"] == "instant_occurred"
    )
    counter_kinds = frozenset(
        event["counter_kind"] for event in events if event["kind"] == "counter_sampled"
    )
    for actual, expected, label in (
        (event_kinds, EVENT_KINDS, "event kinds"),
        (span_kinds, SPAN_KINDS, "span kinds"),
        (instant_kinds, INSTANT_KINDS, "instant kinds"),
        (counter_kinds, COUNTER_KINDS, "counter kinds"),
    ):
        if actual != expected:
            _fail("coverage", label, f"missing={sorted(expected - actual)}, extra={sorted(actual - expected)}")

    component_pairs = {
        (event["detail"]["stage"], event["detail"]["error_code"])
        for event in events
        if event.get("instant_kind") == "diagnostic.component_failed"
    }
    if component_pairs != {
        ("enqueue", "delivery_queue_unavailable"),
        ("callback", "callback_raised"),
        ("callback", "callback_invalid_return"),
    }:
        _fail("coverage", "component failures", "typed sink stage/error coverage is incomplete")
    walked = list(_walk(events))
    if "0" not in walked or str(MAX_U64) not in walked or None not in walked:
        _fail("coverage", "limits", "zero, u64 maximum, or null is absent")
    token_values = [
        int(event[name])
        for event in events
        if event["kind"] == "act_token_usage_finalized"
        for name in (
            "provider_total_tokens",
            "input_tokens",
            "output_tokens",
            "thought_tokens",
            "cached_read_tokens",
            "cached_write_tokens",
        )
        if event[name] is not None
    ]
    if not any(value > MAX_U64 for value in token_values):
        _fail("coverage", "token integers", "no arbitrary-size token integer")
    if not any(isinstance(value, str) and not value.isascii() for value in walked):
        _fail("coverage", "unicode", "no Unicode string")
    if not any(len(event["caused_by"]) >= 2 for event in events):
        _fail("coverage", "causal", "no multi-causal event")
    if not any(
        event["kind"] == "custom_counter_sampled" and event["value"]["type"] == "decimal"
        for event in events
    ):
        _fail("coverage", "custom decimal", "no decimal custom counter")

    scenario = files["nested-overlap.json"]
    starts = {
        int(event["sequence"]): event
        for event in scenario
        if event["kind"] in {"span_started", "custom_span_started"}
    }
    finished = {
        int(event["span_id"])
        for event in scenario
        if event["kind"] in {"span_finished", "custom_span_finished"}
    }
    roots = [event for event in starts.values() if event["parent_span_id"] is None]
    if not any(event["parent_span_id"] is not None for event in starts.values()):
        _fail("coverage", "nested-overlap", "nested span is absent")
    if len(roots) < 2:
        _fail("coverage", "nested-overlap", "overlapping roots are absent")
    if not any(span_id not in finished for span_id in starts):
        _fail("coverage", "nested-overlap", "open span is absent")


def verify_event_fixtures(root: Path) -> VerificationSummary:
    root = root.resolve()
    _, entries = _load_manifest(root)
    files: dict[str, list[dict[str, Any]]] = {}
    malformed_count = 0
    for entry in entries:
        value = _canonical_json(root / entry["file"])
        if entry["format"] == "event_array":
            events = _array(value, entry["file"])
            validate_stream(events, entry["file"])
            files[entry["file"]] = events
        else:
            fixture = _object(value, entry["file"])
            _closed(fixture, frozenset({"cases"}), entry["file"])
            names: set[str] = set()
            for index, raw in enumerate(_array(fixture["cases"], f"{entry['file']}.cases")):
                path = f"{entry['file']}.cases[{index}]"
                case = _object(raw, path)
                _closed(case, frozenset({"name", "expected_error", "event"}), path)
                name = _string(case["name"], f"{path}.name")
                expected = _string(case["expected_error"], f"{path}.expected_error")
                if name in names:
                    _fail("malformed", f"{path}.name", "duplicate case name")
                names.add(name)
                try:
                    validate_event(case["event"], f"{path}.event")
                except FixtureValidationError as error:
                    if error.code != expected:
                        _fail("malformed", path, f"expected {expected}, got {error.code}")
                else:
                    _fail("malformed", path, "case unexpectedly passed independent decode")
                malformed_count += 1

    _assert_required_coverage(files)
    events = [event for values in files.values() for event in values]
    return VerificationSummary(
        fixture_count=len(entries),
        valid_event_count=len(events),
        malformed_case_count=malformed_count,
        event_kinds=frozenset(event["kind"] for event in events),
        span_kinds=frozenset(
            event["span_kind"] for event in events if event["kind"] == "span_started"
        ),
        instant_kinds=frozenset(
            event["instant_kind"] for event in events if event["kind"] == "instant_occurred"
        ),
        counter_kinds=frozenset(
            event["counter_kind"] for event in events if event["kind"] == "counter_sampled"
        ),
    )


def canonical_event_bytes(root: Path, *, reverse: bool) -> dict[str, bytes]:
    _, entries = _load_manifest(root.resolve())
    event_entries = [entry for entry in entries if entry["format"] == "event_array"]
    if reverse:
        event_entries.reverse()
    result: dict[str, bytes] = {}
    for entry in event_entries:
        events = _array(_canonical_json(root / entry["file"]), entry["file"])
        for index, event in enumerate(events):
            validate_event(event, f"{entry['file']}[{index}]")
            result[f"{entry['file']}#{index}"] = json.dumps(
                event, ensure_ascii=False, separators=(",", ":")
            ).encode()
    return result


def _load_view_manifest(root: Path) -> list[dict[str, Any]]:
    manifest = _object(_canonical_json(root / "manifest.json"), "view_manifest")
    _closed(manifest, frozenset({"schema_version", "fixtures"}), "view_manifest")
    if type(manifest["schema_version"]) is not int or manifest["schema_version"] != 1:
        _fail("schema_version", "view_manifest.schema_version", "expected integer 1")
    entries = _array(manifest["fixtures"], "view_manifest.fixtures")
    files: list[str] = []
    for index, raw in enumerate(entries):
        path = f"view_manifest.fixtures[{index}]"
        entry = _object(raw, path)
        _closed(entry, frozenset({"file", "format", "sha256"}), path)
        name = _string(entry["file"], f"{path}.file")
        if Path(name).name != name or name == "manifest.json":
            _fail("manifest", f"{path}.file", "fixture name is not canonical")
        expected_format = VIEW_FIXTURE_FORMATS.get(name)
        if expected_format is None or entry["format"] != expected_format:
            _fail("manifest", f"{path}.format", f"expected {expected_format}")
        digest = _string(entry["sha256"], f"{path}.sha256")
        if HEX_SHA256.fullmatch(digest) is None:
            _fail("sha256", f"{path}.sha256", "digest is not lowercase SHA-256")
        fixture_path = root / name
        try:
            actual = hashlib.sha256(fixture_path.read_bytes()).hexdigest()
        except OSError as error:
            raise FixtureValidationError("manifest", str(fixture_path), str(error)) from error
        if actual != digest:
            _fail("sha256", str(fixture_path), f"expected {digest}, got {actual}")
        files.append(name)
    if tuple(files) != EXPECTED_VIEW_FILES:
        _fail("manifest", "view_manifest.fixtures", "fixture membership or order is not exact")
    return entries


def _view_id(value: Any, path: str) -> str:
    text = _string(value, path)
    if len(text.encode("utf-8")) > 64 or VIEW_ID.fullmatch(text) is None:
        _fail("view_id", path, "expected ^[a-z][a-z0-9_]*$ with at most 64 bytes")
    return text


def _plain_title(value: Any, path: str) -> str:
    text = _string(value, path)
    if (
        not text
        or len(text.encode("utf-8")) > 128
        or any(unicodedata.category(char) == "Cc" for char in text)
    ):
        _fail("title", path, "plain-text title is out of bounds")
    lower = text.lower()
    if (
        any(marker in text for marker in ("<", ">", "`"))
        or any(
            marker in lower
            for marker in ("javascript:", "data:text/html", "http://", "https://", "url(", "@import")
        )
    ):
        _fail("title", path, "title contains executable markup or an external URL")
    return text


def _view_custom_key(value: Any, path: str) -> str:
    key = _custom_key(value, path)
    lower = key.lower()
    if (
        any(unicodedata.category(char) == "Cc" for char in key)
        or any(marker in key for marker in ("<", ">", "`"))
        or any(
            marker in lower
            for marker in ("javascript:", "data:text/html", "http://", "https://", "url(", "@import")
        )
    ):
        _fail("custom_key", path, "key contains executable markup or an external URL")
    return key


def _selector(value: Any, path: str, builtins: frozenset[str]) -> str:
    selector = _object(value, path)
    kind = _enum(selector.get("selector"), frozenset({"built_in", "custom"}), f"{path}.selector")
    if kind == "built_in":
        _closed(selector, frozenset({"selector", "kind"}), path)
        _enum(selector["kind"], builtins, f"{path}.kind")
    else:
        _closed(selector, frozenset({"selector", "name"}), path)
        _custom_name(selector["name"], f"{path}.name")
    return kind


def _group_dimension(value: Any, path: str) -> None:
    dimension = _object(value, path)
    kind = _enum(
        dimension.get("dimension"),
        frozenset(
            {
                "scene", "actor", "cue", "act", "event_name", "custom_name",
                "attribute", "custom_dimension",
            }
        ),
        f"{path}.dimension",
    )
    if kind in {"attribute", "custom_dimension"}:
        _closed(dimension, frozenset({"dimension", "key"}), path)
        _view_custom_key(dimension["key"], f"{path}.key")
    else:
        _closed(dimension, frozenset({"dimension"}), path)


def _query_filter(value: Any, path: str) -> None:
    item = _object(value, path)
    kind = _enum(
        item.get("filter"),
        frozenset({"severity", "outcome", "attribute_equals", "attribute_exists"}),
        f"{path}.filter",
    )
    if kind == "severity":
        _closed(item, frozenset({"filter", "value"}), path)
        _enum(item["value"], frozenset({"debug", "info", "warning", "error"}), f"{path}.value")
    elif kind == "outcome":
        _closed(item, frozenset({"filter", "value"}), path)
        _enum(item["value"], SPAN_OUTCOMES, f"{path}.value")
    elif kind == "attribute_equals":
        _closed(item, frozenset({"filter", "key", "value"}), path)
        _view_custom_key(item["key"], f"{path}.key")
        _tagged_scalar(item["value"], f"{path}.value", attribute=False)
    else:
        _closed(item, frozenset({"filter", "key"}), path)
        _view_custom_key(item["key"], f"{path}.key")


def _query_filters(value: Any, path: str) -> None:
    filters = _array(value, path)
    if len(filters) > 32:
        _fail("filters", path, "more than 32 exact filters")
    for index, item in enumerate(filters):
        _query_filter(item, f"{path}[{index}]")


def _timeline_source(value: Any, path: str) -> str:
    source = _object(value, path)
    kind = _enum(source.get("source"), frozenset({"span", "instant"}), f"{path}.source")
    _closed(source, frozenset({"source", "selector"}), path)
    _selector(
        source["selector"], f"{path}.selector", SPAN_KINDS if kind == "span" else INSTANT_KINDS,
    )
    return kind


def _metric_source(value: Any, path: str) -> str:
    source = _object(value, path)
    kind = _enum(
        source.get("source"),
        frozenset({"counter_value", "instant_count", "completed_span_duration", "act_token"}),
        f"{path}.source",
    )
    if kind == "counter_value":
        _closed(source, frozenset({"source", "selector", "selection"}), path)
        _selector(source["selector"], f"{path}.selector", COUNTER_KINDS)
        if source["selection"] != "latest_before_reduce":
            _fail("counter_selection", f"{path}.selection", "counter must select latest before reduce")
    elif kind == "instant_count":
        _closed(source, frozenset({"source", "selector"}), path)
        _selector(source["selector"], f"{path}.selector", INSTANT_KINDS)
    elif kind == "completed_span_duration":
        _closed(source, frozenset({"source", "selector"}), path)
        _selector(source["selector"], f"{path}.selector", SPAN_KINDS)
    else:
        _closed(source, frozenset({"source", "metric"}), path)
        _enum(source["metric"], TOKEN_METRICS, f"{path}.metric")
    return kind


def _table_source(value: Any, path: str) -> str:
    source = _object(value, path)
    kind = _enum(
        source.get("source"),
        frozenset({"event", "span", "instant", "counter", "act_token_usage"}),
        f"{path}.source",
    )
    if kind == "event":
        _closed(source, frozenset({"source", "kind"}), path)
        _enum(source["kind"], EVENT_KINDS, f"{path}.kind")
    elif kind == "act_token_usage":
        _closed(source, frozenset({"source"}), path)
    else:
        _closed(source, frozenset({"source", "selector"}), path)
        allowed = {"span": SPAN_KINDS, "instant": INSTANT_KINDS, "counter": COUNTER_KINDS}[kind]
        _selector(source["selector"], f"{path}.selector", allowed)
    return kind


def _table_column(value: Any, path: str) -> None:
    column = _object(value, path)
    kind = _enum(
        column.get("column"),
        frozenset(
            {
                "sequence", "elapsed_ns", "event_kind", "span_kind", "instant_kind",
                "counter_kind", "scene_id", "actor_id", "cue_id", "act_id", "custom_name",
                "outcome", "severity", "attribute", "token", "value",
            }
        ),
        f"{path}.column",
    )
    if kind == "attribute":
        _closed(column, frozenset({"column", "key"}), path)
        _view_custom_key(column["key"], f"{path}.key")
    elif kind == "token":
        _closed(column, frozenset({"column", "metric"}), path)
        _enum(column["metric"], TOKEN_METRICS, f"{path}.metric")
    else:
        _closed(column, frozenset({"column"}), path)


def validate_view_record(value: Any, path: str = "view") -> dict[str, Any]:
    record = _object(value, path)
    _closed(
        record,
        frozenset({"renderer", "view_schema_version", "id", "title", "time_range", "scope", "query"}),
        path,
    )
    renderer = _enum(record["renderer"], VIEW_RENDERERS, f"{path}.renderer")
    if type(record["view_schema_version"]) is not int or record["view_schema_version"] != 1:
        _fail("view_schema_version", f"{path}.view_schema_version", "expected integer 1")
    _view_id(record["id"], f"{path}.id")
    _plain_title(record["title"], f"{path}.title")
    _enum(record["time_range"], VIEW_TIME_RANGES, f"{path}.time_range")
    _enum(record["scope"], VIEW_SCOPES, f"{path}.scope")
    query = _object(record["query"], f"{path}.query")
    if renderer == "timeline":
        _closed(query, frozenset({"source", "filters", "group_by"}), f"{path}.query")
        _timeline_source(query["source"], f"{path}.query.source")
        _query_filters(query["filters"], f"{path}.query.filters")
        if query["group_by"] is not None:
            _group_dimension(query["group_by"], f"{path}.query.group_by")
    elif renderer in {"metric", "time_series"}:
        _closed(query, frozenset({"source", "filters", "group_by", "reducer"}), f"{path}.query")
        source = _metric_source(query["source"], f"{path}.query.source")
        reducer = _enum(query["reducer"], VIEW_REDUCERS, f"{path}.query.reducer")
        if source == "instant_count" and reducer != "count":
            _fail("reducer", f"{path}.query.reducer", "instant count only supports count")
        _query_filters(query["filters"], f"{path}.query.filters")
        if query["group_by"] is not None:
            _group_dimension(query["group_by"], f"{path}.query.group_by")
    else:
        _closed(query, frozenset({"source", "filters", "columns", "page_size"}), f"{path}.query")
        _table_source(query["source"], f"{path}.query.source")
        _query_filters(query["filters"], f"{path}.query.filters")
        columns = _array(query["columns"], f"{path}.query.columns")
        if not columns or len(columns) > 32:
            _fail("columns", f"{path}.query.columns", "column count is out of bounds")
        for index, column in enumerate(columns):
            _table_column(column, f"{path}.query.columns[{index}]")
        page_size = query["page_size"]
        if type(page_size) is not int or not 1 <= page_size <= MAX_PAGE_ROWS:
            _fail("page_size", f"{path}.query.page_size", "page size is out of bounds")
    _validate_query_compatibility(renderer, query, f"{path}.query")
    return record


def _validate_query_compatibility(renderer: str, query: dict[str, Any], path: str) -> None:
    source = query["source"]
    source_kind = source["source"]
    selector_kind = source.get("selector", {}).get("selector")
    outcome = source_kind in {"span", "completed_span_duration"}
    severity = source_kind in {"instant", "instant_count"} and selector_kind == "custom"
    scalar_fields = selector_kind == "custom" and source_kind in {
        "span", "instant", "counter", "counter_value", "completed_span_duration", "instant_count",
    }
    custom_name = selector_kind == "custom"
    custom_dimensions = selector_kind == "custom" and source_kind in {"counter", "counter_value"}
    if renderer == "table" and source_kind == "event":
        event_kind = source["kind"]
        outcome = event_kind in {"span_finished", "custom_span_finished"}
        severity = event_kind == "custom_instant_occurred"
        scalar_fields = event_kind in {
            "custom_span_started", "custom_instant_occurred", "custom_counter_sampled",
        }
        custom_name = event_kind.startswith("custom_")
        custom_dimensions = event_kind == "custom_counter_sampled"
    for index, item in enumerate(query["filters"]):
        supported = {
            "outcome": outcome,
            "severity": severity,
            "attribute_equals": scalar_fields,
            "attribute_exists": scalar_fields,
        }[item["filter"]]
        if not supported:
            _fail("filter", f"{path}.filters[{index}]", "filter is incompatible with source")
    group = query.get("group_by")
    if group is not None:
        group_kind = group["dimension"]
        supported = (
            group_kind in {"scene", "actor", "cue", "act", "event_name"}
            or (group_kind == "custom_name" and custom_name)
            or (group_kind == "attribute" and scalar_fields)
            or (group_kind == "custom_dimension" and custom_dimensions)
        )
        if not supported:
            _fail("group", f"{path}.group_by", "group dimension is incompatible with source")


def _capabilities(value: Any, path: str) -> None:
    capabilities = _object(value, path)
    expected = {
        "event_schema_version": 1,
        "view_schema_version": 1,
        "api_schema_version": 1,
        "max_page_rows": 500,
        "max_metric_series": 64,
        "max_time_series_points": 1024,
        "max_time_series_series": 64,
        "bucket_origin": "run",
        "interval_semantics": "left_closed_right_open",
        "counter_selection": "latest_before_reduce",
        "exact_mean_components": True,
    }
    _closed(capabilities, frozenset(expected), path)
    if capabilities != expected:
        _fail("capabilities", path, "operational capability values drifted")


def _binding(value: Any, path: str, record: dict[str, Any]) -> tuple[int, int]:
    binding = _object(value, path)
    _closed(
        binding,
        frozenset(
            {
                "captured_watermark", "captured_elapsed_end_ns", "time_range", "range_start_ns",
                "range_end_ns", "scope", "selected_scope",
            }
        ),
        path,
    )
    _u64(binding["captured_watermark"], f"{path}.captured_watermark")
    captured_end = _u64(binding["captured_elapsed_end_ns"], f"{path}.captured_elapsed_end_ns")
    mode = _enum(binding["time_range"], VIEW_TIME_RANGES, f"{path}.time_range")
    start = _u64(binding["range_start_ns"], f"{path}.range_start_ns")
    end = _u64(binding["range_end_ns"], f"{path}.range_end_ns")
    scope = _enum(binding["scope"], VIEW_SCOPES, f"{path}.scope")
    if start > end or end > captured_end:
        _fail("binding", path, "range lies outside captured data")
    if mode == "run" and (start != 0 or end != captured_end):
        _fail("binding", path, "run range is not [0, captured_end)")
    if mode != record["time_range"] or scope != record["scope"]:
        _fail("binding", path, "response binding does not match descriptor")
    if binding["selected_scope"] is not None:
        selected = _scope(binding["selected_scope"], f"{path}.selected_scope")
        if (
            selected["effect_id"] is not None
            or selected["tool_call_id"] is not None
            or all(selected[field] is None for field in ("scene_id", "actor_id", "cue_id", "act_id"))
        ):
            _fail("binding", f"{path}.selected_scope", "selection is not a domain scope")
    if scope == "run" and binding["selected_scope"] is not None:
        _fail("binding", f"{path}.selected_scope", "run scope cannot contain selection")
    return start, end


def _coverage(value: Any, path: str) -> dict[str, Any]:
    item = _object(value, path)
    _closed(
        item,
        frozenset({"status", "matched_count", "contributing_count", "excluded_count", "excluded", "gap_count"}),
        path,
    )
    status = _enum(item["status"], frozenset({"complete", "partial", "unavailable"}), f"{path}.status")
    matched = _u64(item["matched_count"], f"{path}.matched_count")
    contributing = _u64(item["contributing_count"], f"{path}.contributing_count")
    excluded_count = _u64(item["excluded_count"], f"{path}.excluded_count")
    gaps = _u64(item["gap_count"], f"{path}.gap_count")
    reasons = _object(item["excluded"], f"{path}.excluded")
    _closed(
        reasons,
        frozenset({"open_spans", "missing_values", "non_numeric_values", "unavailable_values", "resource_truncated"}),
        f"{path}.excluded",
    )
    reason_total = sum(_u64(number, f"{path}.excluded.{name}") for name, number in reasons.items())
    if (
        contributing + excluded_count != matched
        or reason_total != excluded_count
    ):
        _fail("coverage", path, "matched, contributing, or excluded counts are inconsistent")
    complete = excluded_count == 0 and gaps == 0
    if (status == "complete" and not complete) or (status == "partial" and complete):
        _fail("coverage", path, "status does not match exclusions and gaps")
    if status == "unavailable" and contributing != 0:
        _fail("coverage", path, "unavailable coverage has contributing values")
    return item


def _pagination(value: Any, path: str) -> dict[str, Any] | None:
    if value is None:
        return None
    page = _object(value, path)
    _closed(page, frozenset({"page_size", "next_cursor"}), path)
    if type(page["page_size"]) is not int or not 1 <= page["page_size"] <= MAX_PAGE_ROWS:
        _fail("page_size", f"{path}.page_size", "page size is out of bounds")
    if page["next_cursor"] is not None:
        cursor = _string(page["next_cursor"], f"{path}.next_cursor")
        if not cursor or len(cursor) > 512 or not cursor.isascii():
            _fail("cursor", f"{path}.next_cursor", "opaque cursor is out of bounds")
    return page


def _exact_number(value: Any, path: str) -> None:
    number = _object(value, path)
    _closed(number, frozenset({"type", "value"}), path)
    kind = _enum(number["type"], frozenset({"integer", "decimal"}), f"{path}.type")
    (_canonical_integer if kind == "integer" else _decimal)(number["value"], f"{path}.value")


def _aggregate(value: Any, path: str) -> str:
    aggregate = _object(value, path)
    kind = _enum(aggregate.get("aggregate"), frozenset({"exact", "mean"}), f"{path}.aggregate")
    if kind == "exact":
        _closed(aggregate, frozenset({"aggregate", "value"}), path)
        _exact_number(aggregate["value"], f"{path}.value")
    else:
        _closed(aggregate, frozenset({"aggregate", "numerator", "contributing_count"}), path)
        _exact_number(aggregate["numerator"], f"{path}.numerator")
        _token_integer(aggregate["contributing_count"], f"{path}.contributing_count")
    return kind


def _group_key(value: Any, path: str) -> dict[str, Any] | None:
    if value is None:
        return None
    group = _object(value, path)
    _closed(group, frozenset({"dimension", "value"}), path)
    _group_dimension(group["dimension"], f"{path}.dimension")
    _tagged_scalar(group["value"], f"{path}.value", attribute=False)
    dimension = group["dimension"]["dimension"]
    scalar_type = group["value"]["type"]
    if dimension in {"scene", "actor", "cue", "act", "event_name", "custom_name"}:
        if scalar_type != "string":
            _fail("group", f"{path}.value", "built-in group value is not a string")
    elif dimension == "custom_dimension" and scalar_type == "null":
        _fail("group", f"{path}.value", "custom dimension group value is null")
    return group


def _response_common(response: dict[str, Any], path: str, record: dict[str, Any]) -> tuple[int, int, dict[str, Any] | None]:
    if type(response["api_schema_version"]) is not int or response["api_schema_version"] != 1:
        _fail("api_schema_version", f"{path}.api_schema_version", "expected integer 1")
    if type(response["view_schema_version"]) is not int or response["view_schema_version"] != 1:
        _fail("view_schema_version", f"{path}.view_schema_version", "expected integer 1")
    _uuid(response["run_id"], f"{path}.run_id")
    view_id = _view_id(response["view_id"], f"{path}.view_id")
    if view_id != record["id"]:
        _fail("view_id", f"{path}.view_id", "result does not identify its descriptor")
    start, end = _binding(response["binding"], f"{path}.binding", record)
    result_coverage = _coverage(response["coverage"], f"{path}.coverage")
    pagination = _pagination(response["pagination"], f"{path}.pagination")
    truncated = _boolean(response["truncated"], f"{path}.truncated")
    resource_truncated = int(result_coverage["excluded"]["resource_truncated"]) > 0
    if truncated != resource_truncated:
        _fail("truncation", path, "truncation state and coverage disagree")
    if response["incompatible"] is not None:
        state = _object(response["incompatible"], f"{path}.incompatible")
        _closed(
            state,
            frozenset({"reason", "supported_view_schema_version", "record_view_schema_version"}),
            f"{path}.incompatible",
        )
        reason = _enum(
            state["reason"],
            frozenset({"newer_view_schema", "corrupt_record"}),
            f"{path}.incompatible.reason",
        )
        if state["supported_view_schema_version"] != 1:
            _fail("view_schema_version", f"{path}.incompatible", "wrong supported version")
        record_version = state["record_view_schema_version"]
        if record_version is not None and type(record_version) is not int:
            _fail("view_schema_version", f"{path}.incompatible", "record version is not an integer")
        if reason == "newer_view_schema" and (
            type(record_version) is not int or record_version <= 1
        ):
            _fail("view_schema_version", f"{path}.incompatible", "newer reason is not newer")
        if reason == "corrupt_record" and type(record_version) is int and record_version > 1:
            _fail("view_schema_version", f"{path}.incompatible", "newer record is not corrupt")
    _capabilities(response["capabilities"], f"{path}.capabilities")
    return start, end, pagination


def validate_view_response(value: Any, record: dict[str, Any], path: str = "response") -> tuple[int, int]:
    response = _object(value, path)
    renderer = _enum(response.get("renderer"), VIEW_RENDERERS, f"{path}.renderer")
    if renderer != record["renderer"]:
        _fail("renderer", f"{path}.renderer", "response renderer differs from descriptor")
    common = {
        "renderer", "api_schema_version", "view_schema_version", "run_id", "view_id", "binding",
        "coverage", "pagination", "truncated", "incompatible", "capabilities",
    }
    data_fields = {
        "timeline": {"rows"},
        "metric": {"series"},
        "table": {"columns", "rows"},
        "time_series": {"bucket_width_ns", "series"},
    }[renderer]
    _closed(response, frozenset(common | data_fields), path)
    start, end, pagination = _response_common(response, path, record)
    incompatible = response["incompatible"] is not None
    if incompatible and response["coverage"]["status"] != "unavailable":
        _fail("incompatible", path, "incompatible result coverage is not unavailable")
    if renderer in {"timeline", "table"}:
        if pagination is None:
            _fail("pagination", f"{path}.pagination", "row renderer requires pagination state")
    elif pagination is not None:
        _fail("pagination", f"{path}.pagination", "aggregate renderer cannot be paginated")

    if renderer == "timeline":
        rows = _array(response["rows"], f"{path}.rows")
        if incompatible and rows:
            _fail("incompatible", f"{path}.rows", "incompatible timeline contains rows")
        if len(rows) > pagination["page_size"]:
            _fail("page_size", f"{path}.rows", "timeline result exceeds page size")
        captured_watermark = int(response["binding"]["captured_watermark"])
        captured_end = int(response["binding"]["captured_elapsed_end_ns"])
        previous_sequence = 0
        for index, raw in enumerate(rows):
            row_path = f"{path}.rows[{index}]"
            row = _object(raw, row_path)
            _closed(
                row,
                frozenset(
                    {"sequence", "group", "item_type", "name", "start_ns", "end_ns", "scope", "outcome"}
                ),
                row_path,
            )
            sequence = _u64(row["sequence"], f"{row_path}.sequence")
            if sequence == 0 or sequence <= previous_sequence or sequence > captured_watermark:
                _fail("sequence", f"{row_path}.sequence", "row is outside the captured ordered prefix")
            previous_sequence = sequence
            group = _group_key(row["group"], f"{row_path}.group")
            expected_group = record["query"]["group_by"]
            if (None if group is None else group["dimension"]) != expected_group:
                _fail("group", f"{row_path}.group", "group key differs from descriptor")
            item_type = _enum(row["item_type"], frozenset({"span", "instant"}), f"{row_path}.item_type")
            name = _string(row["name"], f"{row_path}.name")
            row_start = _u64(row["start_ns"], f"{row_path}.start_ns")
            row_end = _optional(row["end_ns"], _u64, f"{row_path}.end_ns")
            scope = _scope(row["scope"], f"{row_path}.scope")
            if group is not None:
                dimension = group["dimension"]["dimension"]
                if dimension in {"scene", "actor", "cue", "act"}:
                    if group["value"]["value"] != scope[f"{dimension}_id"]:
                        _fail("group", f"{row_path}.group", "group value differs from row scope")
                elif dimension in {"event_name", "custom_name"}:
                    if group["value"]["value"] != name:
                        _fail("group", f"{row_path}.group", "group value differs from row name")
            outcome = _optional(row["outcome"], lambda item, item_path: _enum(item, SPAN_OUTCOMES, item_path), f"{row_path}.outcome")
            if item_type == "instant" and (row_end is not None or outcome is not None):
                _fail("timeline", row_path, "instant contains span-only fields")
            if row_end is not None and row_end < row_start:
                _fail("timeline", row_path, "span ends before it starts")
            if item_type == "span" and ((row_end is None) != (outcome is None)):
                _fail("timeline", row_path, "span completion and outcome disagree")
            if row_start > captured_end or (row_end is not None and row_end > captured_end):
                _fail("binding", row_path, "timeline row lies beyond captured time")
            intersects = (
                start <= row_start < end
                if item_type == "instant"
                else start < end and row_start < end and (captured_end if row_end is None else row_end) > start
            )
            if not intersects:
                _fail("binding", row_path, "timeline row does not intersect query range")
            selected_scope = response["binding"]["selected_scope"]
            if selected_scope is not None and not _scope_contains(selected_scope, scope):
                _fail("binding", f"{row_path}.scope", "row lies outside selected scope")
            source = record["query"]["source"]
            expected_item_type = source["source"]
            selector = source["selector"]
            expected_name = selector.get("kind", selector.get("name"))
            if item_type != expected_item_type or name != expected_name:
                _fail("source", row_path, "timeline row differs from query source")
        return len(rows), 0

    if renderer == "metric":
        series = _array(response["series"], f"{path}.series")
        if incompatible and series:
            _fail("incompatible", f"{path}.series", "incompatible metric contains series")
        if len(series) > MAX_METRIC_SERIES:
            _fail("series_cap", f"{path}.series", "metric series count exceeds 64")
        seen_groups: set[str] = set()
        for index, raw in enumerate(series):
            series_path = f"{path}.series[{index}]"
            item = _object(raw, series_path)
            _closed(item, frozenset({"group", "value", "coverage"}), series_path)
            group = _group_key(item["group"], f"{series_path}.group")
            group_identity = json.dumps(group, sort_keys=True, separators=(",", ":"))
            if group_identity in seen_groups:
                _fail("group", f"{series_path}.group", "duplicate metric group")
            seen_groups.add(group_identity)
            expected_group = record["query"]["group_by"]
            if (None if group is None else group["dimension"]) != expected_group:
                _fail("group", f"{series_path}.group", "group key differs from descriptor")
            item_coverage = _coverage(item["coverage"], f"{series_path}.coverage")
            if item["value"] is None:
                if int(item_coverage["contributing_count"]) != 0:
                    _fail("coverage", series_path, "empty metric has contributing values")
            else:
                aggregate_kind = _aggregate(item["value"], f"{series_path}.value")
                expected_kind = "mean" if record["query"]["reducer"] == "mean" else "exact"
                if aggregate_kind != expected_kind:
                    _fail("reducer", f"{series_path}.value", "aggregate shape differs from reducer")
                if record["query"]["reducer"] == "count":
                    exact = item["value"]["value"]
                    if exact["type"] != "integer" or exact["value"].startswith("-"):
                        _fail("reducer", f"{series_path}.value", "count is not a nonnegative integer")
                if record["query"]["source"]["source"] in {"completed_span_duration", "act_token"}:
                    exact = (
                        item["value"]["numerator"]
                        if aggregate_kind == "mean"
                        else item["value"]["value"]
                    )
                    if exact["type"] != "integer" or exact["value"].startswith("-"):
                        _fail("source", f"{series_path}.value", "integral source has non-integral value")
                if aggregate_kind == "mean" and int(item["value"]["contributing_count"]) != int(item_coverage["contributing_count"]):
                    _fail("coverage", series_path, "mean count differs from contributing coverage")
        return 0, 0

    if renderer == "table":
        columns = _array(response["columns"], f"{path}.columns")
        if columns != record["query"]["columns"]:
            _fail("columns", f"{path}.columns", "response columns differ from descriptor")
        for index, column in enumerate(columns):
            _table_column(column, f"{path}.columns[{index}]")
        rows = _array(response["rows"], f"{path}.rows")
        if incompatible and rows:
            _fail("incompatible", f"{path}.rows", "incompatible table contains rows")
        if len(rows) > pagination["page_size"] or len(rows) > MAX_PAGE_ROWS:
            _fail("page_size", f"{path}.rows", "table result exceeds page size")
        captured_watermark = int(response["binding"]["captured_watermark"])
        previous_sequence = 0
        for index, raw in enumerate(rows):
            row_path = f"{path}.rows[{index}]"
            row = _object(raw, row_path)
            _closed(row, frozenset({"sequence", "cells"}), row_path)
            sequence = _u64(row["sequence"], f"{row_path}.sequence")
            if sequence == 0 or sequence <= previous_sequence or sequence > captured_watermark:
                _fail("sequence", f"{row_path}.sequence", "row is outside the captured ordered prefix")
            previous_sequence = sequence
            cells = _array(row["cells"], f"{row_path}.cells")
            if len(cells) != len(columns):
                _fail("columns", f"{row_path}.cells", "cell count differs from columns")
            for cell_index, cell in enumerate(cells):
                if cell is not None:
                    _tagged_scalar(cell, f"{row_path}.cells[{cell_index}]", attribute=False)
        return len(rows), 0

    width = _u64(response["bucket_width_ns"], f"{path}.bucket_width_ns")
    duration = end - start
    expected_width = max(1, (duration + 1022) // 1023) if duration else 1
    if width != expected_width:
        _fail("bucket_width", f"{path}.bucket_width_ns", f"expected {expected_width}")
    expected_buckets: list[tuple[int, int, bool]] = []
    if start != end:
        bucket_start = start // width * width
        while bucket_start < end:
            bucket_end = bucket_start + width
            expected_buckets.append((bucket_start, bucket_end, bucket_start < start or bucket_end > end))
            bucket_start = bucket_end
    if len(expected_buckets) > MAX_TIME_SERIES_POINTS:
        _fail("point_cap", path, "more than 1024 origin-aligned buckets")
    series = _array(response["series"], f"{path}.series")
    if incompatible and series:
        _fail("incompatible", f"{path}.series", "incompatible time-series contains series")
    if len(series) > MAX_TIME_SERIES_SERIES:
        _fail("series_cap", f"{path}.series", "time-series count exceeds 64")
    max_points = 0
    seen_groups = set()
    for series_index, raw in enumerate(series):
        series_path = f"{path}.series[{series_index}]"
        item = _object(raw, series_path)
        _closed(item, frozenset({"group", "points"}), series_path)
        group = _group_key(item["group"], f"{series_path}.group")
        group_identity = json.dumps(group, sort_keys=True, separators=(",", ":"))
        if group_identity in seen_groups:
            _fail("group", f"{series_path}.group", "duplicate time-series group")
        seen_groups.add(group_identity)
        expected_group = record["query"]["group_by"]
        if (None if group is None else group["dimension"]) != expected_group:
            _fail("group", f"{series_path}.group", "group key differs from descriptor")
        points = _array(item["points"], f"{series_path}.points")
        max_points = max(max_points, len(points))
        if len(points) != len(expected_buckets):
            _fail("buckets", f"{series_path}.points", "empty or intersecting bucket is missing")
        for point_index, (raw_point, expected) in enumerate(zip(points, expected_buckets, strict=True)):
            point_path = f"{series_path}.points[{point_index}]"
            point = _object(raw_point, point_path)
            _closed(
                point,
                frozenset({"bucket_start_ns", "bucket_end_ns", "partial", "value", "coverage"}),
                point_path,
            )
            actual = (
                _u64(point["bucket_start_ns"], f"{point_path}.bucket_start_ns"),
                _u64(point["bucket_end_ns"], f"{point_path}.bucket_end_ns"),
                _boolean(point["partial"], f"{point_path}.partial"),
            )
            if actual != expected:
                _fail("buckets", point_path, f"expected origin-aligned bucket {expected}")
            point_coverage = _coverage(point["coverage"], f"{point_path}.coverage")
            if point["value"] is None:
                if int(point_coverage["contributing_count"]) != 0:
                    _fail("coverage", point_path, "empty bucket has contributing values")
            else:
                aggregate_kind = _aggregate(point["value"], f"{point_path}.value")
                expected_kind = "mean" if record["query"]["reducer"] == "mean" else "exact"
                if aggregate_kind != expected_kind:
                    _fail("reducer", f"{point_path}.value", "aggregate shape differs from reducer")
                if record["query"]["reducer"] == "count":
                    exact = point["value"]["value"]
                    if exact["type"] != "integer" or exact["value"].startswith("-"):
                        _fail("reducer", f"{point_path}.value", "count is not a nonnegative integer")
                if record["query"]["source"]["source"] in {"completed_span_duration", "act_token"}:
                    exact = (
                        point["value"]["numerator"]
                        if aggregate_kind == "mean"
                        else point["value"]["value"]
                    )
                    if exact["type"] != "integer" or exact["value"].startswith("-"):
                        _fail("source", f"{point_path}.value", "integral source has non-integral value")
                if aggregate_kind == "mean" and int(point["value"]["contributing_count"]) != int(point_coverage["contributing_count"]):
                    _fail("coverage", point_path, "mean count differs from contributing coverage")
    return 0, max_points


def verify_view_fixtures(root: Path) -> ViewVerificationSummary:
    root = root.resolve()
    entries = _load_view_manifest(root)
    renderers: set[str] = set()
    invalid_count = 0
    max_table_rows = 0
    max_time_series_points = 0
    renderer_payloads: dict[str, dict[str, Any]] = {}
    for entry in entries:
        name = entry["file"]
        value = _canonical_json(root / name)
        if entry["format"] == "renderer_fixture":
            fixture = _object(value, name)
            _closed(fixture, frozenset({"descriptor", "response"}), name)
            record = validate_view_record(fixture["descriptor"], f"{name}.descriptor")
            renderer = record["renderer"]
            rows, points = validate_view_response(fixture["response"], record, f"{name}.response")
            renderers.add(renderer)
            renderer_payloads[renderer] = fixture
            max_table_rows = max(max_table_rows, rows if renderer == "table" else 0)
            max_time_series_points = max(max_time_series_points, points)
        elif entry["format"] == "compatible":
            fixture = _object(value, name)
            _closed(fixture, frozenset({"capabilities", "records"}), name)
            _capabilities(fixture["capabilities"], f"{name}.capabilities")
            records = _array(fixture["records"], f"{name}.records")
            compatible_renderers = {
                validate_view_record(record, f"{name}.records[{index}]")["renderer"]
                for index, record in enumerate(records)
            }
            if compatible_renderers != VIEW_RENDERERS or len(records) != 4:
                _fail("coverage", f"{name}.records", "compatible record set is not the four-renderer union")
        elif entry["format"] == "invalid_descriptors":
            fixture = _object(value, name)
            _closed(fixture, frozenset({"cases"}), name)
            cases = _array(fixture["cases"], f"{name}.cases")
            expected_names = (
                "sql", "regex", "join", "nested_path", "callable", "custom_renderer",
                "executable_markup", "incompatible_reducer", "page_size_over_limit",
            )
            if tuple(case.get("name") for case in cases if isinstance(case, dict)) != expected_names:
                _fail("coverage", f"{name}.cases", "forbidden descriptor coverage drifted")
            for index, raw in enumerate(cases):
                case_path = f"{name}.cases[{index}]"
                case = _object(raw, case_path)
                _closed(case, frozenset({"name", "record"}), case_path)
                try:
                    validate_view_record(case["record"], f"{case_path}.record")
                except FixtureValidationError:
                    pass
                else:
                    _fail("invalid_descriptor", case_path, "forbidden descriptor decoded")
                invalid_count += 1
        else:
            fixture = _object(value, name)
            _closed(fixture, frozenset({"record", "expected_reason"}), name)
            reason = _enum(
                fixture["expected_reason"],
                frozenset({"newer_view_schema", "corrupt_record"}),
                f"{name}.expected_reason",
            )
            record = _object(fixture["record"], f"{name}.record")
            version = record.get("view_schema_version")
            if reason == "newer_view_schema":
                if type(version) is not int or version <= 1:
                    _fail("view_schema_version", f"{name}.record", "newer fixture is not newer")
                if record.get("event_schema_version") != 1 or record.get("api_schema_version") != 1:
                    _fail("version_independence", f"{name}.record", "event/API versions changed with view")
            else:
                if version != 1:
                    _fail("view_schema_version", f"{name}.record", "corrupt fixture is not current schema")
                try:
                    validate_view_record(record, f"{name}.record")
                except FixtureValidationError:
                    pass
                else:
                    _fail("corrupt_record", f"{name}.record", "corrupt record decoded")

    if renderers != VIEW_RENDERERS:
        _fail("coverage", "view renderers", "four-renderer fixture coverage is incomplete")
    if max_table_rows != MAX_PAGE_ROWS or max_time_series_points != MAX_TIME_SERIES_POINTS:
        _fail("coverage", "view limits", "500-row or 1024-point boundary is absent")
    if renderer_payloads["timeline"]["response"]["rows"]:
        _fail("coverage", "timeline empty", "empty row result is absent")
    metric_series = renderer_payloads["metric"]["response"]["series"]
    if not any(
        series["coverage"]["status"] == "partial"
        and series["value"] is not None
        and series["value"]["aggregate"] == "mean"
        for series in metric_series
    ):
        _fail("coverage", "metric", "partial exact-mean result is absent")
    points = renderer_payloads["time_series"]["response"]["series"][0]["points"]
    if not points[0]["partial"] or not points[-1]["partial"] or not any(point["value"] is None for point in points):
        _fail("coverage", "time series", "partial boundaries or explicit empty buckets are absent")
    source = renderer_payloads["time_series"]["descriptor"]["query"]["source"]
    if source.get("selection") != "latest_before_reduce":
        _fail("coverage", "counter", "latest-before-reduce counter source is absent")
    return ViewVerificationSummary(
        fixture_count=len(entries),
        renderers=frozenset(renderers),
        invalid_case_count=invalid_count,
        max_table_rows=max_table_rows,
        max_time_series_points=max_time_series_points,
    )


def canonical_view_bytes(root: Path, *, reverse: bool) -> dict[str, bytes]:
    root = root.resolve()
    entries = _load_view_manifest(root)
    verify_view_fixtures(root)
    if reverse:
        entries.reverse()
    return {
        entry["file"]: json.dumps(
            _canonical_json(root / entry["file"]), ensure_ascii=False, separators=(",", ":")
        ).encode()
        for entry in entries
    }


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="Verify canonical diagnostic protocol fixtures")
    parser.add_argument(
        "--views",
        action="store_true",
        help="verify the closed view/query protocol fixtures instead of event fixtures",
    )
    parser.add_argument(
        "--fixtures",
        type=Path,
        default=None,
    )
    arguments = parser.parse_args(argv)
    root = arguments.fixtures or (
        Path(__file__).resolve().parents[1]
        / "tests/fixtures/diagnostics"
        / ("views" if arguments.views else "events")
    )
    try:
        summary = verify_view_fixtures(root) if arguments.views else verify_event_fixtures(root)
    except FixtureValidationError as error:
        print(f"fixture verification failed: {error}", file=sys.stderr)
        return 1
    if isinstance(summary, ViewVerificationSummary):
        print(
            f"verified {summary.fixture_count} view fixture files "
            f"({len(summary.renderers)} renderers, {summary.invalid_case_count} invalid descriptors)"
        )
    else:
        print(
            f"verified {summary.fixture_count} fixture files "
            f"({summary.valid_event_count} events, {summary.malformed_case_count} malformed cases)"
        )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
