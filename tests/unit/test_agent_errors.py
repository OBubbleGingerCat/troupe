from __future__ import annotations

import pytest

import troupe
import troupe._runtime as runtime


def test_complete_agent_error_hierarchy_is_public_and_native() -> None:
    expected_bases = {
        "AgentError": RuntimeError,
        "AgentSessionBusyError": troupe.AgentError,
        "AgentSessionError": troupe.AgentError,
        "AgentSessionStartError": troupe.AgentSessionError,
        "AgentAuthenticationRequiredError": troupe.AgentSessionStartError,
        "AgentSessionBrokenError": troupe.AgentSessionError,
        "AgentTurnError": troupe.AgentError,
        "AgentResultError": troupe.AgentTurnError,
        "AgentResultMissingError": troupe.AgentResultError,
    }

    for name, base in expected_bases.items():
        error_type = getattr(troupe, name)
        assert error_type is getattr(runtime, name)
        assert error_type.__module__ == "troupe"
        assert issubclass(error_type, base)

    assert not issubclass(troupe.AgentSessionBusyError, troupe.AgentSessionError)
    assert not issubclass(troupe.AgentTurnError, troupe.AgentSessionError)
    assert not issubclass(troupe.AgentResultError, troupe.AgentSessionError)


def test_agent_result_issue_is_a_native_immutable_factory_only_value() -> None:
    issue_type = troupe.AgentResultIssue

    assert issue_type is runtime.AgentResultIssue
    assert issue_type.__module__ == "troupe"
    with pytest.raises(TypeError):
        issue_type()
    with pytest.raises(TypeError):
        type("MutableIssue", (issue_type,), {})


def test_schema_programming_errors_are_not_top_level_agent_errors() -> None:
    assert not hasattr(troupe, "ValueRejected")
    assert not hasattr(troupe, "SchemaCallbackError")
    assert issubclass(troupe.act_schema.ValueRejected, ValueError)
    assert issubclass(troupe.act_schema.SchemaCallbackError, RuntimeError)
    assert not issubclass(troupe.act_schema.SchemaCallbackError, troupe.AgentError)


def test_schema_callback_error_has_read_only_phase_and_path() -> None:
    error = troupe.act_schema.SchemaCallbackError(
        "callback failed",
        phase="validate",
        path="/result/value",
    )

    assert error.args == ("callback failed",)
    assert error.phase == "validate"
    assert error.path == "/result/value"
    with pytest.raises(AttributeError):
        error.phase = "render_prompt"  # type: ignore[misc]
    with pytest.raises(AttributeError):
        error.path = "/other"  # type: ignore[misc]
    with pytest.raises(AttributeError):
        error._phase = "render_prompt"  # type: ignore[attr-defined]
    with pytest.raises(AttributeError):
        error._path = "/other"  # type: ignore[attr-defined]
    with pytest.raises(AttributeError):
        object.__setattr__(error, "_phase", "render_prompt")
    with pytest.raises(AttributeError):
        object.__setattr__(error, "_path", "/other")
    assert error.phase == "validate"
    assert error.path == "/result/value"
