from __future__ import annotations

import asyncio
import importlib
import json
import sys
from pathlib import Path
from typing import Any

import pytest

import troupe


ROOT = Path(__file__).resolve().parents[2]
MOCK_AGENT = ROOT / "tests" / "support" / "mock_acp_agent.py"
HARNESS_TIMEOUT = 5.0


def _native() -> Any:
    return importlib.import_module("troupe._runtime")


def _events(path: Path) -> list[dict[str, Any]]:
    if not path.exists():
        return []
    return [json.loads(line) for line in path.read_text(encoding="utf-8").splitlines()]


def _launch(
    tmp_path: Path,
    scenario: str,
    *,
    results: list[dict[str, object]] | None = None,
) -> tuple[Path, Path]:
    events = tmp_path / "events.jsonl"
    workspace = tmp_path / "workspace"
    workspace.mkdir()
    args = [
        str(MOCK_AGENT),
        "--events",
        str(events),
        "--scenario",
        scenario,
    ]
    if results is not None:
        args.extend(
            ["--results-json", json.dumps(results, separators=(",", ":"))]
        )
    _native()._agent_test_set_launch(program=sys.executable, args=args)
    return events, workspace


def _profile(workspace: Path) -> troupe.AgentProfile:
    return troupe.AgentProfile(
        agent="codex",
        workspace=workspace,
        model="test-model",
        effort="max",
    )


def _run(runtime: Any, production: troupe.Production) -> None:
    async def scenario() -> None:
        await asyncio.wait_for(
            asyncio.shield(runtime.run(production)),
            HARNESS_TIMEOUT,
        )

    asyncio.run(scenario())


@pytest.fixture(autouse=True)
def _reset_test_launch() -> Any:
    _native()._agent_test_reset_launch()
    yield
    _native()._agent_test_reset_launch()


def test_tool_envelope_errors_do_not_consume_budget_and_first_valid_wins(
    tmp_path: Path,
) -> None:
    events, workspace = _launch(tmp_path, "act_result_matrix")
    observed: list[dict[str, object]] = []
    runtime = _native()._Runtime()

    class MatrixActor(troupe.Actor):
        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            del cue
            observed.append(
                await self.act(
                    script="Choose one decision.",
                    output_schema={
                        "decision": troupe.act_schema.StrValue(
                            description="decision",
                            choices=("approve", "reject"),
                        )
                    },
                )
            )
            return ()

    class MatrixProduction(troupe.Production):
        async def scene(self) -> None:
            handle = self.cast_actor(
                MatrixActor,
                name="matrix",
                agent_profile=_profile(workspace),
                actor_args=(),
                actor_kwargs={},
            )
            assert await handle.cue({}) == ()
            runtime.request_shutdown()

    _run(runtime, MatrixProduction([]))

    assert observed == [{"decision": "approve"}]
    matrix = next(row for row in _events(events) if row["event"] == "result_matrix_complete")
    assert matrix["unauthorized_status"] == 401


def test_http_body_limit_is_exact_and_chunked_decoding_uses_same_budget(
    tmp_path: Path,
) -> None:
    events, workspace = _launch(tmp_path, "act_body_limits")
    observed: list[dict[str, object]] = []
    runtime = _native()._Runtime()

    class BodyActor(troupe.Actor):
        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            del cue
            observed.append(
                await self.act(
                    script="Exercise the result body limit.",
                    output_schema={
                        "decision": troupe.act_schema.StrValue(
                            description="decision",
                            choices=("approve", "reject"),
                        )
                    },
                )
            )
            return ()

    class BodyProduction(troupe.Production):
        async def scene(self) -> None:
            handle = self.cast_actor(
                BodyActor,
                name="body-limits",
                agent_profile=_profile(workspace),
                actor_args=(),
                actor_kwargs={},
            )
            assert await handle.cue({}) == ()
            runtime.request_shutdown()

    _run(runtime, BodyProduction([]))

    assert observed == [{"decision": "approve"}]
    assert any(row["event"] == "body_limit_matrix_complete" for row in _events(events))


def test_ninth_invalid_result_is_terminal_and_retains_bounded_evidence(
    tmp_path: Path,
) -> None:
    events, workspace = _launch(
        tmp_path,
        "act_submit_results",
        results=[{"decision": "maybe"}] * 9,
    )
    captured: list[troupe.AgentResultError] = []
    runtime = _native()._Runtime()

    class InvalidActor(troupe.Actor):
        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            del cue
            try:
                await self.act(
                    script="Submit invalid decisions.",
                    output_schema={
                        "decision": troupe.act_schema.StrValue(
                            description="decision",
                            choices=("approve", "reject"),
                        )
                    },
                )
            except troupe.AgentResultError as error:
                captured.append(error)
            else:
                raise AssertionError("ninth invalid result unexpectedly succeeded")
            return ()

    class InvalidProduction(troupe.Production):
        async def scene(self) -> None:
            handle = self.cast_actor(
                InvalidActor,
                name="invalid",
                agent_profile=_profile(workspace),
                actor_args=(),
                actor_kwargs={},
            )
            assert await handle.cue({}) == ()
            runtime.request_shutdown()

    _run(runtime, InvalidProduction([]))

    assert len(captured) == 1
    error = captured[0]
    assert type(error) is troupe.AgentResultError
    assert error.code == "too_many_invalid_results"
    assert error.invalid_calls == 9
    assert error.details_truncated is False
    assert [(issue.path, issue.code) for issue in error.issues] == [
        ("/decision", "not_in_choices")
    ]
    tool_events = [row for row in _events(events) if row["event"] == "tool_result_received"]
    assert len(tool_events) == 9
    assert "rejected after 9 invalid calls" in tool_events[-1]["text"]


def test_validation_issue_collection_keeps_first_sixteen_in_path_order(
    tmp_path: Path,
) -> None:
    submitted = {f"field_{index:02d}": index for index in reversed(range(18))}
    _, workspace = _launch(
        tmp_path,
        "act_submit_results",
        results=[submitted],
    )
    captured: list[troupe.AgentResultError] = []
    runtime = _native()._Runtime()

    class BoundedActor(troupe.Actor):
        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            del cue
            try:
                await self.act(
                    script="Submit extra fields.",
                    output_schema={},
                )
            except troupe.AgentResultError as error:
                captured.append(error)
            else:
                raise AssertionError("extra fields unexpectedly succeeded")
            return ()

    class BoundedProduction(troupe.Production):
        async def scene(self) -> None:
            handle = self.cast_actor(
                BoundedActor,
                name="bounded-issues",
                agent_profile=_profile(workspace),
                actor_args=(),
                actor_kwargs={},
            )
            assert await handle.cue({}) == ()
            runtime.request_shutdown()

    _run(runtime, BoundedProduction([]))

    error = captured[0]
    assert error.code == "invalid_result"
    assert error.invalid_calls == 1
    assert error.details_truncated is True
    assert len(error.issues) == 16
    assert [issue.path for issue in error.issues] == [
        f"/field_{index:02d}" for index in range(16)
    ]
    assert {issue.code for issue in error.issues} == {"extra_field"}


def test_end_turn_without_any_tool_call_is_missing_result(tmp_path: Path) -> None:
    _, workspace = _launch(tmp_path, "act_no_result")
    captured: list[troupe.AgentResultMissingError] = []
    runtime = _native()._Runtime()

    class MissingActor(troupe.Actor):
        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            del cue
            try:
                await self.act(
                    script="End without a result.",
                    output_schema={
                        "value": troupe.act_schema.Int64Value(
                            description="missing integer"
                        )
                    },
                )
            except troupe.AgentResultMissingError as error:
                captured.append(error)
            else:
                raise AssertionError("missing result unexpectedly succeeded")
            return ()

    class MissingProduction(troupe.Production):
        async def scene(self) -> None:
            handle = self.cast_actor(
                MissingActor,
                name="missing",
                agent_profile=_profile(workspace),
                actor_args=(),
                actor_kwargs={},
            )
            assert await handle.cue({}) == ()
            runtime.request_shutdown()

    _run(runtime, MissingProduction([]))

    assert len(captured) == 1
    error = captured[0]
    assert type(error) is troupe.AgentResultMissingError
    assert error.code == "missing_result"
    assert error.issues == ()
    assert error.invalid_calls == 0
    assert error.details_truncated is False


@pytest.mark.parametrize(
    "scenario",
    [
        "act_request_error_then_success",
        "act_request_error_transport_collision_then_success",
        "act_request_error_parse_collision_then_success",
        "act_request_error_internal_collision_then_success",
    ],
)
def test_correlated_prompt_error_is_request_failed_and_session_remains_reusable(
    tmp_path: Path,
    scenario: str,
) -> None:
    events, workspace = _launch(tmp_path, scenario)
    captured: list[troupe.AgentTurnError] = []
    observed: list[dict[str, object]] = []
    states: list[str] = []
    runtime = _native()._Runtime()

    class RequestErrorActor(troupe.Actor):
        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            if cue.instruction["turn"] == 1:
                try:
                    await self.act(
                        script="Return a correlated request error.",
                        output_schema={
                            "value": troupe.act_schema.Int64Value(
                                description="integer result"
                            )
                        },
                    )
                except troupe.AgentTurnError as error:
                    captured.append(error)
                else:
                    raise AssertionError("prompt request error unexpectedly succeeded")
            else:
                observed.append(
                    await self.act(
                        script="Reuse the same session after the request error.",
                        output_schema={
                            "value": troupe.act_schema.Int64Value(
                                description="integer result"
                            )
                        },
                    )
                )
            return ()

    class RequestErrorProduction(troupe.Production):
        async def scene(self) -> None:
            handle = self.cast_actor(
                RequestErrorActor,
                name="request-error",
                agent_profile=_profile(workspace),
                actor_args=(),
                actor_kwargs={},
            )
            assert await handle.cue({"turn": 1}) == ()
            states.append(handle._agent_state_for_test())  # type: ignore[attr-defined]
            assert await handle.cue({"turn": 2}) == ()
            states.append(handle._agent_state_for_test())  # type: ignore[attr-defined]
            runtime.request_shutdown()

    _run(runtime, RequestErrorProduction([]))

    assert len(captured) == 1
    assert type(captured[0]) is troupe.AgentTurnError
    assert captured[0].code == "request_failed"
    assert captured[0].__cause__ is None
    assert "mock prompt" not in str(captured[0])
    assert observed == [{"value": 7}]
    assert states == ["ready", "ready"]
    prompts = [row for row in _events(events) if row["event"] == "prompt_received"]
    assert len(prompts) == 2
    assert len({row["session_id"] for row in prompts}) == 1


@pytest.mark.parametrize(
    ("scenario", "expected_code", "invalid_calls"),
    [
        ("act_invalid_then_request_error", "invalid_result", 1),
        ("act_ninth_invalid_then_request_error", "too_many_invalid_results", 9),
    ],
)
def test_result_rejection_precedes_a_correlated_prompt_error(
    tmp_path: Path,
    scenario: str,
    expected_code: str,
    invalid_calls: int,
) -> None:
    events, workspace = _launch(tmp_path, scenario)
    captured: list[troupe.AgentResultError] = []
    states: list[str] = []
    runtime = _native()._Runtime()

    class RejectedActor(troupe.Actor):
        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            del cue
            try:
                await self.act(
                    script="Preserve invalid-result evidence across the prompt error.",
                    output_schema={
                        "value": troupe.act_schema.Int64Value(
                            description="integer result"
                        )
                    },
                )
            except troupe.AgentResultError as error:
                captured.append(error)
            else:
                raise AssertionError("prompt error erased the invalid-result outcome")
            return ()

    class RejectedProduction(troupe.Production):
        async def scene(self) -> None:
            handle = self.cast_actor(
                RejectedActor,
                name="rejected-before-request-error",
                agent_profile=_profile(workspace),
                actor_args=(),
                actor_kwargs={},
            )
            assert await handle.cue({}) == ()
            while handle._agent_state_for_test() == "cancelling":  # type: ignore[attr-defined]
                await asyncio.sleep(0)
            states.append(handle._agent_state_for_test())  # type: ignore[attr-defined]
            runtime.request_shutdown()

    _run(runtime, RejectedProduction([]))

    assert len(captured) == 1
    assert captured[0].code == expected_code
    assert captured[0].invalid_calls == invalid_calls
    assert captured[0].__cause__ is None
    assert states == ["ready"]
    tool_events = [row for row in _events(events) if row["event"] == "tool_result_received"]
    assert len(tool_events) == invalid_calls
    assert all(row["is_error"] is True for row in tool_events)


def test_transport_loss_precedes_a_repairable_invalid_result(tmp_path: Path) -> None:
    events, workspace = _launch(tmp_path, "act_invalid_then_transport_loss")
    captured: list[BaseException] = []
    states: list[str] = []
    runtime = _native()._Runtime()

    class InvalidActor(troupe.Actor):
        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            del cue
            try:
                await self.act(
                    script="Return one invalid value, then lose the transport.",
                    output_schema={
                        "value": troupe.act_schema.Int64Value(
                            description="integer result"
                        )
                    },
                )
            except (troupe.AgentResultError, troupe.AgentSessionBrokenError) as error:
                captured.append(error)
            return ()

    class InvalidProduction(troupe.Production):
        async def scene(self) -> None:
            handle = self.cast_actor(
                InvalidActor,
                name="repairable-invalid-before-transport-loss",
                agent_profile=_profile(workspace),
                actor_args=(),
                actor_kwargs={},
            )
            assert await handle.cue({}) == ()
            states.append(handle._agent_state_for_test())  # type: ignore[attr-defined]
            runtime.request_shutdown()

    _run(runtime, InvalidProduction([]))

    assert len(captured) == 1
    assert type(captured[0]) is troupe.AgentSessionBrokenError
    assert captured[0].code == "transport_lost"  # type: ignore[attr-defined]
    assert states == ["broken"]
    tool_events = [row for row in _events(events) if row["event"] == "tool_result_received"]
    assert len(tool_events) == 1
    assert tool_events[0]["is_error"] is True
    assert any(row["event"] == "transport_closing_after_invalid" for row in _events(events))


def test_schema_callback_failure_precedes_a_correlated_prompt_error(
    tmp_path: Path,
) -> None:
    events, workspace = _launch(tmp_path, "act_callback_fault_then_request_error")
    cause = LookupError("private callback failure")
    captured: list[troupe.act_schema.SchemaCallbackError] = []
    states: list[str] = []
    runtime = _native()._Runtime()

    class FailingValue(troupe.act_schema.SchemaValue[int]):
        def __init__(self) -> None:
            super().__init__(description="failing integer", json_kind="int64")

        def render_prompt(self) -> str:
            return "must be an integer"

        def validate(self, value: int) -> None:
            del value
            raise cause

    class CallbackActor(troupe.Actor):
        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            del cue
            try:
                await self.act(
                    script="Preserve the callback fault across the prompt error.",
                    output_schema={"value": FailingValue()},
                )
            except troupe.act_schema.SchemaCallbackError as error:
                captured.append(error)
            else:
                raise AssertionError("prompt error erased the callback failure")
            return ()

    class CallbackProduction(troupe.Production):
        async def scene(self) -> None:
            handle = self.cast_actor(
                CallbackActor,
                name="callback-before-request-error",
                agent_profile=_profile(workspace),
                actor_args=(),
                actor_kwargs={},
            )
            assert await handle.cue({}) == ()
            while handle._agent_state_for_test() == "cancelling":  # type: ignore[attr-defined]
                await asyncio.sleep(0)
            states.append(handle._agent_state_for_test())  # type: ignore[attr-defined]
            runtime.request_shutdown()

    _run(runtime, CallbackProduction([]))

    assert len(captured) == 1
    assert captured[0].phase == "validate"
    assert captured[0].path == "/value"
    assert captured[0].__cause__ is cause
    assert states == ["ready"]
    tool_event = next(
        row for row in _events(events) if row["event"] == "tool_result_received"
    )
    assert tool_event["is_error"] is True
    assert tool_event["text"] == "schema validation callback failed"


def test_malformed_prompt_response_breaks_session_and_blocks_reuse(tmp_path: Path) -> None:
    events, workspace = _launch(tmp_path, "act_malformed_prompt_response")
    captured: list[troupe.AgentSessionBrokenError] = []
    states: list[str] = []
    runtime = _native()._Runtime()

    class MalformedResponseActor(troupe.Actor):
        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            del cue
            try:
                await self.act(
                    script="Reject an unclassifiable prompt response.",
                    output_schema={
                        "value": troupe.act_schema.Int64Value(
                            description="unreachable integer result"
                        )
                    },
                )
            except troupe.AgentSessionBrokenError as error:
                captured.append(error)
            else:
                raise AssertionError("malformed prompt response left the session usable")
            return ()

    class MalformedResponseProduction(troupe.Production):
        async def scene(self) -> None:
            handle = self.cast_actor(
                MalformedResponseActor,
                name="malformed-response",
                agent_profile=_profile(workspace),
                actor_args=(),
                actor_kwargs={},
            )
            assert await handle.cue({"turn": 1}) == ()
            states.append(handle._agent_state_for_test())  # type: ignore[attr-defined]
            assert await handle.cue({"turn": 2}) == ()
            states.append(handle._agent_state_for_test())  # type: ignore[attr-defined]
            runtime.request_shutdown()

    _run(runtime, MalformedResponseProduction([]))

    assert [error.code for error in captured] == [
        "protocol_violation",
        "protocol_violation",
    ]
    assert all(error.__cause__ is None for error in captured)
    assert states == ["broken", "broken"]
    assert [row["event"] for row in _events(events)].count("prompt_received") == 1
    assert [row["event"] for row in _events(events)].count(
        "malformed_prompt_response_sent"
    ) == 1


def test_accepted_result_with_non_end_turn_discards_value_and_returns_turn_error(
    tmp_path: Path,
) -> None:
    _, workspace = _launch(tmp_path, "act_accepted_non_end_turn")
    captured: list[troupe.AgentTurnError] = []
    states: list[str] = []
    runtime = _native()._Runtime()

    class NonEndTurnActor(troupe.Actor):
        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            del cue
            try:
                await self.act(
                    script="Accept a result, then stop for max tokens.",
                    output_schema={
                        "value": troupe.act_schema.Int64Value(
                            description="discarded integer result"
                        )
                    },
                )
            except troupe.AgentTurnError as error:
                captured.append(error)
            else:
                raise AssertionError("non-end-turn response returned an accepted value")
            return ()

    class NonEndTurnProduction(troupe.Production):
        async def scene(self) -> None:
            handle = self.cast_actor(
                NonEndTurnActor,
                name="non-end-turn",
                agent_profile=_profile(workspace),
                actor_args=(),
                actor_kwargs={},
            )
            assert await handle.cue({}) == ()
            states.append(handle._agent_state_for_test())  # type: ignore[attr-defined]
            runtime.request_shutdown()

    _run(runtime, NonEndTurnProduction([]))

    assert len(captured) == 1
    assert type(captured[0]) is troupe.AgentTurnError
    assert captured[0].code == "max_tokens"
    assert states == ["ready"]


def test_custom_object_preserves_arbitrary_size_integer_tokens_as_python_int(
    tmp_path: Path,
) -> None:
    huge = 10**100 + 123456789
    negative_huge = -(10**120 + 987654321)
    _, workspace = _launch(
        tmp_path,
        "act_submit_results",
        results=[{"value": {"huge": huge, "negative_huge": negative_huge}}],
    )
    callback_values: list[object] = []
    observed: list[dict[str, object]] = []
    runtime = _native()._Runtime()

    class JsonObjectValue(troupe.act_schema.SchemaValue[dict[str, object]]):
        def __init__(self) -> None:
            super().__init__(description="object with an exact integer", json_kind="object")

        def render_prompt(self) -> str:
            return "the huge field must remain an exact JSON integer"

        def validate(self, value: dict[str, object]) -> None:
            callback_values.append(value["huge"])
            assert type(value["huge"]) is int
            assert value["huge"] == huge
            assert type(value["negative_huge"]) is int
            assert value["negative_huge"] == negative_huge

    class HugeIntegerActor(troupe.Actor):
        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            del cue
            observed.append(
                await self.act(
                    script="Return the exact arbitrary-size integer.",
                    output_schema={"value": JsonObjectValue()},
                )
            )
            return ()

    class HugeIntegerProduction(troupe.Production):
        async def scene(self) -> None:
            handle = self.cast_actor(
                HugeIntegerActor,
                name="huge-integer",
                agent_profile=_profile(workspace),
                actor_args=(),
                actor_kwargs={},
            )
            assert await handle.cue({}) == ()
            runtime.request_shutdown()

    _run(runtime, HugeIntegerProduction([]))

    assert callback_values == [huge]
    assert observed == [
        {"value": {"huge": huge, "negative_huge": negative_huge}}
    ]


def test_custom_object_integer_materialization_ignores_python_digit_limit(
    tmp_path: Path,
) -> None:
    positive = 10**5_000 - 1
    negative = -(10**5_001 - 1)
    _, workspace = _launch(tmp_path, "act_digit_limit_integer")
    callback_values: list[tuple[int, int]] = []
    observed: list[dict[str, object]] = []
    runtime = _native()._Runtime()

    class JsonObjectValue(troupe.act_schema.SchemaValue[dict[str, object]]):
        def __init__(self) -> None:
            super().__init__(
                description="object with integers beyond Python's string digit limit",
                json_kind="object",
            )

        def render_prompt(self) -> str:
            return "preserve each arbitrary-size JSON integer exactly"

        def validate(self, value: dict[str, object]) -> None:
            huge = value["huge"]
            negative_huge = value["negative_huge"]
            assert type(huge) is int
            assert type(negative_huge) is int
            assert huge == positive
            assert negative_huge == negative
            callback_values.append((huge, negative_huge))

    class DigitLimitActor(troupe.Actor):
        async def cued(self, cue: troupe.Cue) -> tuple[troupe.Effect, ...]:
            del cue
            observed.append(
                await self.act(
                    script="Return both exact arbitrary-size integers.",
                    output_schema={"value": JsonObjectValue()},
                )
            )
            return ()

    class DigitLimitProduction(troupe.Production):
        async def scene(self) -> None:
            handle = self.cast_actor(
                DigitLimitActor,
                name="digit-limit-integer",
                agent_profile=_profile(workspace),
                actor_args=(),
                actor_kwargs={},
            )
            assert await handle.cue({}) == ()
            runtime.request_shutdown()

    _run(runtime, DigitLimitProduction([]))

    assert callback_values == [(positive, negative)]
    assert observed == [
        {"value": {"huge": positive, "negative_huge": negative}}
    ]
