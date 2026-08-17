"""Semantic assertions shared by the diagnostics happy-path E2E runner."""

from __future__ import annotations

import json
from collections import defaultdict
from pathlib import Path
from typing import Any, Iterable


class OracleError(AssertionError):
    pass


def require(condition: object, detail: str) -> None:
    if not condition:
        raise OracleError(detail)


def read_jsonl(path: Path) -> list[dict[str, Any]]:
    require(path.is_file(), f"missing JSONL file: {path}")
    rows: list[dict[str, Any]] = []
    for number, line in enumerate(path.read_text(encoding="utf-8").splitlines(), 1):
        try:
            value = json.loads(line)
        except json.JSONDecodeError as error:
            raise OracleError(f"{path}:{number}: invalid JSON: {error}") from error
        require(isinstance(value, dict), f"{path}:{number}: expected a JSON object")
        rows.append(value)
    return rows


def assert_dense_events(
    events: list[dict[str, Any]], run_id: str, *, prefix: bool = False
) -> None:
    require(events, "canonical event stream is empty")
    sequences = [int(event["sequence"]) for event in events]
    expected = list(range(1, sequences[-1] + 1))
    if prefix:
        require(sequences == expected, "captured event prefix is not dense")
    else:
        require(sequences == expected, "archive event stream is not exactly dense")
    require(
        {event.get("run_id") for event in events} == {run_id},
        "event stream mixes Run identities",
    )
    require(
        all(event.get("schema_version") == 1 for event in events),
        "event stream contains a non-v1 event",
    )


def _events_of(
    events: Iterable[dict[str, Any]], kind: str
) -> list[dict[str, Any]]:
    return [event for event in events if event.get("kind") == kind]


def assert_full_chain(
    events: list[dict[str, Any]],
    sink_rows: list[dict[str, Any]],
    *,
    provider: str,
    capture_tool_payloads: bool,
    usage_availability: str,
) -> None:
    starts = _events_of(events, "span_started")
    span_kinds = {event.get("span_kind") for event in starts}
    for kind in (
        "run.lifecycle",
        "scene.lifecycle",
        "actor.handle_lifetime",
        "cue.execution",
        "effect.lifecycle",
        "agent.session.lifecycle",
        "act.lifecycle",
        "agent.turn",
        "agent.thinking",
        "tool.call",
    ):
        require(kind in span_kinds, f"missing {kind} span")

    scopes = [event["scope"] for event in events]
    scene_ids = {scope["scene_id"] for scope in scopes if scope["scene_id"] is not None}
    actor_ids = {scope["actor_id"] for scope in scopes if scope["actor_id"] is not None}
    cue_ids = {scope["cue_id"] for scope in scopes if scope["cue_id"] is not None}
    effect_ids = {scope["effect_id"] for scope in scopes if scope["effect_id"] is not None}
    act_ids = {scope["act_id"] for scope in scopes if scope["act_id"] is not None}
    require(len(scene_ids) >= 2, "expected at least two Scene identities")
    require(len(actor_ids) == 1, "expected one persistent Actor identity")
    require(len(cue_ids) == 3, "expected exactly three Cue identities")
    require(len(effect_ids) == 3, "expected exactly three Effect identities")
    require(len(act_ids) == 3, "expected exactly three Act identities")

    event_kinds = {event["kind"] for event in events}
    for kind in (
        "agent_message_delta",
        "agent_message_completed",
        "agent_plan_snapshot",
        "context_usage_sampled",
        "act_token_usage_finalized",
        "custom_span_started",
        "custom_span_finished",
        "custom_instant_occurred",
        "custom_counter_sampled",
    ):
        require(kind in event_kinds, f"missing {kind} event")

    counters: dict[tuple[str, str], list[tuple[int, int]]] = defaultdict(list)
    for event in _events_of(events, "counter_sampled"):
        act_id = event["scope"]["act_id"]
        counter_kind = event["counter_kind"]
        if act_id is not None:
            counters[(act_id, counter_kind)].append(
                (int(event["sequence"]), int(event["value"]))
            )
    active_groups = [
        values
        for (act_id, kind), values in counters.items()
        if act_id in act_ids and kind == "agent.turn.active"
    ]
    require(len(active_groups) == 3, "agent.turn.active is not scoped to all Acts")
    for values in active_groups:
        ordered = [value for _, value in sorted(values)]
        require(ordered == [1, 0], f"invalid agent.turn.active pair: {ordered}")

    rejection_groups = {
        act_id: [value for _, value in sorted(values)]
        for (act_id, kind), values in counters.items()
        if kind == "result.validation_rejections"
    }
    require(
        sorted(rejection_groups.values(), key=len) == [[1], [1, 2], [1, 2, 3]],
        f"invalid result rejection samples: {rejection_groups}",
    )

    usages = _events_of(events, "act_token_usage_finalized")
    require(len(usages) == 3, "expected one finalized usage event per Act")
    require(
        {event["availability"] for event in usages} == {usage_availability},
        f"unexpected {provider} usage qualification",
    )
    if usage_availability == "available":
        require(
            [int(event["input_tokens"]) for event in usages] == [20, 40, 60],
            "qualified terminal usage was not preserved per Act",
        )
    else:
        require(
            all(event["input_tokens"] is None for event in usages),
            "unqualified provider leaked terminal usage",
        )

    tool_events = [
        event
        for event in events
        if event.get("span_kind") == "tool.call"
        or event.get("instant_kind") == "tool.call.updated"
    ]
    require(tool_events, "canonical stream has no tool lifecycle")
    for event in tool_events:
        detail = event.get("detail", {})
        require(detail.get("captured_input") is None, "canonical store leaked tool input")
        require(detail.get("captured_output") is None, "canonical store leaked tool output")

    sink_events = [row for row in sink_rows if row.get("record") == "event"]
    summaries = [row for row in sink_rows if row.get("record") == "summary"]
    require(len(summaries) == 3, "expected three closed sink summaries")
    require(
        [summary["result"] for summary in summaries]
        == [{"value": 1}, {"value": 2}, {"value": 3}],
        "Actor.act no longer returns only validated dictionaries",
    )
    require(
        all(
            summary["act_outcome"] == "completed"
            and summary["close_reason"] == "act_finished"
            and summary["complete"] is True
            for summary in summaries
        ),
        "sink did not close cleanly with its Act",
    )
    canonical_by_sequence = {int(event["sequence"]): event for event in events}
    sink_sequences = [int(row["sequence"]) for row in sink_events]
    require(sink_sequences == sorted(sink_sequences), "sink sequence order is not canonical")
    require(len(sink_sequences) == len(set(sink_sequences)), "sink duplicated a sequence")
    for row in sink_events:
        sequence = int(row["sequence"])
        require(sequence in canonical_by_sequence, f"sink sequence {sequence} is not durable")
        require(
            row["kind"] == canonical_by_sequence[sequence]["kind"],
            f"sink/store kind mismatch at sequence {sequence}",
        )

    sink_text = json.dumps(sink_events, sort_keys=True, separators=(",", ":"))
    if capture_tool_payloads:
        for turn in (1, 2, 3):
            require(
                f"input-is-content-{turn}" in sink_text,
                f"opt-in sink missed tool input for Act {turn}",
            )
            require(
                f"output-is-content-{turn}" in sink_text,
                f"opt-in sink missed tool output for Act {turn}",
            )
    else:
        require("input-is-content" not in sink_text, "capture-off sink retained tool input")
        require("output-is-content" not in sink_text, "capture-off sink retained tool output")

    sink_counter_samples = {
        (row.get("scope", {}).get("act_id"), row.get("counter_kind"), row.get("value"))
        for row in sink_events
        if row.get("kind") == "counter_sampled"
    }
    for (act_id, kind), values in counters.items():
        if kind not in {"agent.turn.active", "result.validation_rejections"}:
            continue
        for _, value in values:
            require(
                (act_id, kind, value) in sink_counter_samples,
                f"sink missed {kind}={value} for {act_id}",
            )


def assert_view_catalog(catalog: dict[str, Any], run_id: str) -> None:
    require(catalog.get("run_id") == run_id, "view catalog Run mismatch")
    views = catalog.get("views")
    require(isinstance(views, list), "view catalog has no view list")
    require(
        {view["renderer"] for view in views}
        == {"timeline", "metric", "table", "time_series"},
        "view catalog does not contain all four closed renderers",
    )
    capabilities = catalog.get("capabilities", {})
    require(capabilities.get("bucket_origin") == "run", "TimeSeries origin drifted")
    require(
        capabilities.get("interval_semantics") == "left_closed_right_open",
        "TimeSeries interval semantics drifted",
    )
    require(
        capabilities.get("max_time_series_points") == 1024,
        "TimeSeries point limit drifted",
    )


def assert_view_response(response: dict[str, Any], renderer: str, run_id: str) -> None:
    require(response.get("run_id") == run_id, "view response Run mismatch")
    require(response.get("renderer") == renderer, f"expected {renderer} response")
    require(response.get("incompatible") is None, f"{renderer} view is incompatible")
    require(response.get("truncated") is False, f"{renderer} view was truncated")
    binding = response.get("binding", {})
    require(int(binding.get("captured_watermark", "0")) > 0, "empty view binding")
    if renderer == "time_series":
        points = sum(len(series["points"]) for series in response.get("series", []))
        require(points <= 1024, "TimeSeries exceeded its point limit")
        require(
            all(
                int(point["bucket_start_ns"]) < int(point["bucket_end_ns"])
                for series in response.get("series", [])
                for point in series["points"]
            ),
            "TimeSeries emitted an invalid bucket interval",
        )


def assert_perfetto_summary(
    summary: dict[str, Any], run_id: str, expected_through: int
) -> None:
    metadata = summary["metadata"]
    require(metadata["run_id"] == run_id, "Perfetto metadata Run mismatch")
    require(
        int(metadata["exported_through"]) == expected_through,
        "Perfetto export does not cover the requested canonical prefix",
    )
    require(summary["unknown_field_count"] == 0, "Perfetto trace has unknown fields")
