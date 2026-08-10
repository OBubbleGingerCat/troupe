from __future__ import annotations

import asyncio
import inspect
import math
from collections.abc import Callable, Generator
from typing import Any

import pytest

from troupe import act_schema


def _accepts(value: act_schema.SchemaValue[Any], candidate: object) -> None:
    assert value.validate(candidate) is None  # type: ignore[arg-type]


def _rejects(value: act_schema.SchemaValue[Any], candidate: object) -> None:
    with pytest.raises(act_schema.ValueRejected):
        value.validate(candidate)  # type: ignore[arg-type]


def test_schema_module_exports_only_the_programming_contract() -> None:
    assert act_schema.__all__ == [
        "BoolValue",
        "Field",
        "Float64Value",
        "Int64Value",
        "ListValue",
        "NullableValue",
        "ObjectValue",
        "SchemaCallbackError",
        "SchemaValue",
        "StrValue",
        "ValueRejected",
    ]
    assert inspect.isabstract(act_schema.SchemaValue)
    for name in act_schema.__all__:
        assert getattr(act_schema, name).__module__ == "troupe.act_schema"


@pytest.mark.parametrize(
    ("factory", "json_kind"),
    [
        (lambda: act_schema.StrValue(description="text"), "string"),
        (lambda: act_schema.Int64Value(description="integer"), "int64"),
        (lambda: act_schema.Float64Value(description="number"), "float64"),
        (lambda: act_schema.BoolValue(description="flag"), "bool"),
        (
            lambda: act_schema.ListValue(
                act_schema.StrValue(description="item"),
                description="items",
            ),
            "array",
        ),
        (
            lambda: act_schema.ObjectValue(
                description="record",
                fields={"value": act_schema.StrValue(description="value")},
            ),
            "object",
        ),
    ],
)
def test_builtin_values_are_immutable_schema_values(
    factory: Callable[[], act_schema.SchemaValue[Any]],
    json_kind: str,
) -> None:
    value = factory()

    assert isinstance(value, act_schema.SchemaValue)
    assert value.json_kind == json_kind
    assert isinstance(value.description, str)
    assert not hasattr(value, "__dict__")
    assert isinstance(value.render_prompt(), str)
    assert value.render_prompt().strip()

    with pytest.raises(AttributeError):
        value.description = "replacement"  # type: ignore[misc]
    with pytest.raises(AttributeError):
        value.extra = True  # type: ignore[attr-defined]


@pytest.mark.parametrize("description", ["", " ", "\t\n", 1, None, "\ud800"])
def test_description_is_required_exact_nonblank_unicode(description: object) -> None:
    with pytest.raises((TypeError, ValueError, UnicodeError)):
        act_schema.StrValue(description=description)  # type: ignore[arg-type]


def test_description_preserves_original_unicode_without_normalization() -> None:
    description = "  e\u0301 result  "
    value = act_schema.StrValue(description=description)

    assert value.description == description


@pytest.mark.parametrize(
    ("description", "encoded_bytes"),
    [
        ("x" * 1_023, 1_023),
        ("x" * 1_024, 1_024),
        ("\U0001f642" * 1_023 + "\u20ac", 4_095),
        ("\U0001f642" * 1_024, 4_096),
    ],
)
def test_description_accepts_exact_scalar_and_utf8_boundaries(
    description: str,
    encoded_bytes: int,
) -> None:
    value = act_schema.StrValue(description=description)

    assert value.description == description
    assert len(value.description.encode("utf-8")) == encoded_bytes


@pytest.mark.parametrize(
    "description",
    [
        "x" * 1_025,
        "\U0001f642" * 1_024 + "x",
    ],
)
def test_description_rejects_exact_scalar_and_utf8_n_plus_one(
    description: str,
) -> None:
    with pytest.raises(ValueError, match="ResourceLimitsV1"):
        act_schema.StrValue(description=description)


@pytest.mark.parametrize(
    "kwargs",
    [
        {"min_length": -1},
        {"max_length": -1},
        {"min_length": 3, "max_length": 2},
        {"min_length": True},
        {"max_length": 1.0},
        {"max_length": 1_048_577},
    ],
)
def test_string_constraints_reject_invalid_bounds(kwargs: dict[str, object]) -> None:
    with pytest.raises((TypeError, ValueError)):
        act_schema.StrValue(description="text", **kwargs)  # type: ignore[arg-type]


def test_string_validation_is_strict_and_enforces_bounds_and_choices() -> None:
    value = act_schema.StrValue(
        description="decision",
        min_length=2,
        max_length=4,
        choices=["go", "stay"],
    )

    for candidate in ("go", "stay"):
        _accepts(value, candidate)
    for candidate in ("g", "later", "stop", 1, True, None):
        _rejects(value, candidate)


@pytest.mark.parametrize(
    "choices",
    [
        [],
        "abc",
        ["a", "a"],
        ["ok", 1],
        ["too long"],
        ["\ud800"],
        [str(index) for index in range(257)],
    ],
)
def test_string_choices_are_nonempty_typed_unique_and_bounded(
    choices: object,
) -> None:
    with pytest.raises((TypeError, ValueError, UnicodeError)):
        act_schema.StrValue(
            description="choice",
            max_length=3,
            choices=choices,  # type: ignore[arg-type]
        )


@pytest.mark.parametrize("count", [255, 256])
def test_string_choices_accept_exact_per_descriptor_boundaries(count: int) -> None:
    value = act_schema.StrValue(
        description="choice",
        choices=[str(index) for index in range(count)],
    )

    for index in range(count):
        _accepts(value, str(index))


@pytest.mark.parametrize("maximum", [1_048_575, 1_048_576])
def test_string_maximum_accepts_runtime_boundaries(maximum: int) -> None:
    value = act_schema.StrValue(description="text", max_length=maximum)

    assert f"maximum length {maximum}" in value.render_prompt()


@pytest.mark.parametrize(
    ("factory", "kwargs"),
    [
        (act_schema.Int64Value, {"min": True}),
        (act_schema.Int64Value, {"max": 1.0}),
        (act_schema.Int64Value, {"min": 2, "max": 1}),
        (act_schema.Int64Value, {"min": -(2**63) - 1}),
        (act_schema.Int64Value, {"max": 2**63}),
        (act_schema.Float64Value, {"min": True}),
        (act_schema.Float64Value, {"max": math.inf}),
        (act_schema.Float64Value, {"min": math.nan}),
        (act_schema.Float64Value, {"min": 2.0, "max": 1.0}),
    ],
)
def test_numeric_constraints_reject_invalid_bounds(
    factory: Callable[..., act_schema.SchemaValue[Any]],
    kwargs: dict[str, object],
) -> None:
    with pytest.raises((TypeError, ValueError)):
        factory(description="number", **kwargs)


def test_integer_none_choice_is_a_typed_input_error_not_an_assertion() -> None:
    with pytest.raises(TypeError, match="integer choice"):
        act_schema.Int64Value(description="integer", choices=[None])  # type: ignore[list-item]


def test_int64_validation_rejects_bool_float_and_out_of_range() -> None:
    value = act_schema.Int64Value(
        description="score",
        min=-2,
        max=2,
        choices=(-2, 0, 2),
    )

    for candidate in (-2, 0, 2):
        _accepts(value, candidate)
    for candidate in (-3, -1, 1, 3, False, 0.0, "0", 2**63):
        _rejects(value, candidate)


def test_float64_validation_canonicalizes_integer_numbers_but_not_bool() -> None:
    value = act_schema.Float64Value(
        description="confidence",
        min=0,
        max=2.0,
        choices=(0.5, 1, 2.0),
    )

    for candidate in (0.5, 1, 1.0, 2, 2.0):
        _accepts(value, candidate)
    for candidate in (False, -1, 1.5, math.nan, math.inf, "1"):
        _rejects(value, candidate)


@pytest.mark.parametrize(
    "choices",
    [
        [],
        [True],
        [1, 1],
        [1, 1.0],
        [math.nan],
        [math.inf],
        [2**1024],
    ],
)
def test_float_choices_reject_invalid_or_canonically_duplicate_values(
    choices: object,
) -> None:
    with pytest.raises((TypeError, ValueError, OverflowError)):
        act_schema.Float64Value(
            description="number",
            choices=choices,  # type: ignore[arg-type]
        )


def test_bool_validation_and_choices_never_use_integer_equality() -> None:
    value = act_schema.BoolValue(description="enabled", choices=[True])

    _accepts(value, True)
    for candidate in (False, 1, 0, "true", None):
        _rejects(value, candidate)

    with pytest.raises((TypeError, ValueError)):
        act_schema.BoolValue(description="flag", choices=[True, 1])  # type: ignore[list-item]


@pytest.mark.parametrize(
    "kwargs",
    [
        {"min_items": -1},
        {"max_items": -1},
        {"min_items": 2, "max_items": 1},
        {"min_items": True},
        {"max_items": 1.0},
        {"max_items": 10_001},
    ],
)
def test_list_constraints_reject_invalid_bounds(kwargs: dict[str, object]) -> None:
    with pytest.raises((TypeError, ValueError)):
        act_schema.ListValue(
            act_schema.StrValue(description="item"),
            description="items",
            **kwargs,  # type: ignore[arg-type]
        )


@pytest.mark.parametrize("maximum", [9_999, 10_000])
def test_list_maximum_accepts_runtime_boundaries(maximum: int) -> None:
    value = act_schema.ListValue(
        act_schema.StrValue(description="item"),
        description="items",
        max_items=maximum,
    )

    assert f"maximum items {maximum}" in value.render_prompt()


def test_list_validation_is_strict_recursive_and_bounded() -> None:
    value = act_schema.ListValue(
        act_schema.Int64Value(description="item", min=0, max=2),
        description="items",
        min_items=1,
        max_items=2,
    )

    for candidate in ([0], [1, 2]):
        _accepts(value, candidate)
    for candidate in ([], [0, 1, 2], [3], [True], (1,), "1", None):
        _rejects(value, candidate)


def test_composite_validation_propagates_awaitable_custom_children() -> None:
    completed: list[int] = []

    class ValidationAwaitable:
        def __init__(self, value: int) -> None:
            self.value = value

        def __await__(self) -> Generator[object, None, None]:
            async def complete() -> None:
                await asyncio.sleep(0)
                completed.append(self.value)

            return complete().__await__()

    class AsyncIntValue(act_schema.SchemaValue[int]):
        def __init__(self) -> None:
            super().__init__(description="async integer", json_kind="int64")

        def render_prompt(self) -> str:
            return "must be an integer"

        def validate(self, value: int, /) -> ValidationAwaitable:
            return ValidationAwaitable(value)

    async def validate() -> None:
        item = AsyncIntValue()
        list_result = act_schema.ListValue(item, description="items").validate([1, 2])
        assert inspect.isawaitable(list_result)
        await list_result

        object_result = act_schema.ObjectValue(
            description="record",
            fields={"value": item},
        ).validate({"value": 3})
        assert inspect.isawaitable(object_result)
        await object_result

    asyncio.run(validate())

    assert completed == [1, 2, 3]


def test_optional_and_nullable_preserve_three_distinct_states() -> None:
    schema = act_schema.ObjectValue(
        description="record",
        fields={
            "required": act_schema.StrValue(description="required"),
            "optional": act_schema.Field(
                act_schema.StrValue(description="optional"),
                required=False,
            ),
            "nullable": act_schema.NullableValue(
                act_schema.StrValue(description="nullable")
            ),
            "optional_nullable": act_schema.Field(
                act_schema.NullableValue(
                    act_schema.StrValue(description="optional nullable")
                ),
                required=False,
            ),
        },
    )

    _accepts(schema, {"required": "yes", "nullable": None})
    _accepts(
        schema,
        {
            "required": "yes",
            "optional": "present",
            "nullable": "present",
            "optional_nullable": None,
        },
    )
    for candidate in (
        {"required": "yes"},
        {"required": None, "nullable": None},
        {"required": "yes", "nullable": None, "extra": True},
    ):
        _rejects(schema, candidate)


def test_nullable_inherits_inner_metadata_and_only_adds_nullability() -> None:
    inner = act_schema.Int64Value(description="count", min=1)
    value = act_schema.NullableValue(inner)

    assert value.description == "count"
    assert value.json_kind == "int64"
    _accepts(value, None)
    _accepts(value, 1)
    _rejects(value, 0)


def test_field_and_container_inputs_are_snapshotted_at_construction() -> None:
    choices = ["first", "second"]
    fields: dict[str, act_schema.FieldSpec] = {
        "decision": act_schema.StrValue(
            description="decision",
            choices=choices,
        )
    }
    value = act_schema.ObjectValue(description="result", fields=fields)

    choices[:] = ["replacement"]
    fields.clear()

    _accepts(value, {"decision": "first"})
    _rejects(value, {"decision": "replacement"})
    _rejects(value, {})


@pytest.mark.parametrize(
    "invalid",
    [None, 1, [], (), {"value": object()}],
)
def test_object_fields_require_an_exact_string_keyed_dict(invalid: object) -> None:
    with pytest.raises((TypeError, ValueError)):
        act_schema.ObjectValue(
            description="record",
            fields=invalid,  # type: ignore[arg-type]
        )


def test_field_requires_schema_value_and_exact_required_bool() -> None:
    value = act_schema.StrValue(description="value")
    field = act_schema.Field(value, required=False)

    assert not hasattr(field, "__dict__")
    with pytest.raises(AttributeError):
        field.required = True  # type: ignore[misc]
    with pytest.raises(TypeError):
        act_schema.Field(object(), required=True)  # type: ignore[arg-type]
    with pytest.raises(TypeError):
        act_schema.Field(value, required=1)  # type: ignore[arg-type]


def test_object_value_rejects_field_subclasses_before_snapshotting() -> None:
    class FieldSubclass(act_schema.Field[str]):
        pass

    with pytest.raises(TypeError, match="Field subclasses"):
        act_schema.ObjectValue(
            description="record",
            fields={
                "value": FieldSubclass(
                    act_schema.StrValue(description="value"),
                    required=True,
                )
            },
        )
