from __future__ import annotations

import inspect
from typing import Any

import pytest

from troupe import act_schema


class EvenIntValue(act_schema.SchemaValue[int]):
    def __init__(self, *, description: str) -> None:
        super().__init__(description=description, json_kind="int64")
        self.validation_calls: list[int] = []

    def render_prompt(self) -> str:
        return "must be even"

    def validate(self, value: int) -> None:
        self.validation_calls.append(value)
        if value % 2:
            raise act_schema.ValueRejected("value must be even")


def test_schema_value_is_abstract_generic_and_directly_subclassable() -> None:
    assert inspect.isabstract(act_schema.SchemaValue)
    with pytest.raises(TypeError):
        act_schema.SchemaValue(description="value", json_kind="string")  # type: ignore[abstract]

    class MissingValidate(act_schema.SchemaValue[str]):
        def render_prompt(self) -> str:
            return "text"

    class MissingRender(act_schema.SchemaValue[str]):
        def validate(self, value: str) -> None:
            del value

    with pytest.raises(TypeError):
        MissingValidate(description="value", json_kind="string")
    with pytest.raises(TypeError):
        MissingRender(description="value", json_kind="string")

    value = EvenIntValue(description="an even integer")
    assert value.description == "an even integer"
    assert value.json_kind == "int64"
    assert value.render_prompt() == "must be even"
    assert value.validate(2) is None
    with pytest.raises(act_schema.ValueRejected, match="value must be even"):
        value.validate(3)
    assert value.validation_calls == [2, 3]


def test_custom_value_base_metadata_is_read_only_but_subclass_state_is_open() -> None:
    value = EvenIntValue(description="value")

    with pytest.raises(AttributeError):
        value.description = "replacement"  # type: ignore[misc]
    with pytest.raises(AttributeError):
        value.json_kind = "string"  # type: ignore[misc]

    value.validation_calls.append(4)
    value.extra_state = {"allowed": True}  # type: ignore[attr-defined]
    assert value.extra_state == {"allowed": True}  # type: ignore[attr-defined]


@pytest.mark.parametrize("description", ["", " ", 1, None, "\udfff"])
def test_custom_value_reuses_exact_description_validation(description: object) -> None:
    with pytest.raises((TypeError, ValueError, UnicodeError)):
        EvenIntValue(description=description)  # type: ignore[arg-type]


@pytest.mark.parametrize(
    "json_kind",
    ["", "integer", "null", "any", 1, None, object()],
)
def test_custom_value_requires_a_supported_native_json_kind(json_kind: object) -> None:
    class CustomValue(act_schema.SchemaValue[Any]):
        def render_prompt(self) -> str:
            return "constraint"

        def validate(self, value: Any) -> None:
            del value

    with pytest.raises((TypeError, ValueError)):
        CustomValue(description="value", json_kind=json_kind)  # type: ignore[arg-type]


@pytest.mark.parametrize(
    "json_kind",
    ["string", "int64", "float64", "bool", "array", "object"],
)
def test_custom_value_accepts_each_native_prechecked_json_kind(json_kind: str) -> None:
    class CustomValue(act_schema.SchemaValue[Any]):
        def render_prompt(self) -> str:
            return "constraint"

        def validate(self, value: Any) -> None:
            del value

    value = CustomValue(description="value", json_kind=json_kind)  # type: ignore[arg-type]
    assert value.json_kind == json_kind


def test_value_rejected_is_the_controlled_local_validation_signal() -> None:
    error = act_schema.ValueRejected("not accepted")

    assert isinstance(error, ValueError)
    assert error.args == ("not accepted",)
    assert str(error) == "not accepted"
    assert error.__module__ == "troupe.act_schema"

    for arguments in [(), ("first", "second"), (1,), (None,)]:
        with pytest.raises(TypeError):
            act_schema.ValueRejected(*arguments)  # type: ignore[arg-type]
