use std::collections::{HashMap, HashSet};
use std::ffi::CString;

use pyo3::IntoPyObjectExt as _;
use pyo3::class::gc::{PyTraverseError, PyVisit};
use pyo3::exceptions::{PyTypeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{
    PyAny, PyBool, PyDict, PyFloat, PyInt, PyList, PyModule, PyModuleMethods, PyString, PyTuple,
    PyType,
};
use serde_json::Value as JsonValue;

const ACT_SCHEMA_API: &str = r####"
from __future__ import annotations

import json as _json
import math as _math
import asyncio as _asyncio
import concurrent.futures as _concurrent_futures
import inspect as _inspect
import weakref as _weakref
from abc import ABC as _ABC, abstractmethod as _abstractmethod
from collections.abc import Awaitable as _Awaitable, Sequence as _Sequence
from typing import Any as _Any, Generic as _Generic, TypeVar as _TypeVar, final as _final


_DESCRIPTION_MAX_SCALARS = 1_024
_DESCRIPTION_MAX_BYTES = 4 * 1_024
_STRING_MAX_SCALARS = 1_048_576
_STRING_MAX_BYTES = 4 * 1_024 * 1_024
_LIST_MAX_ITEMS = 10_000
_CHOICES_MAX_ITEMS = 256
_INT64_MIN = -(2**63)
_INT64_MAX = 2**63 - 1
_JSON_KINDS = frozenset({"string", "int64", "float64", "bool", "array", "object"})

_SchemaValueT = _TypeVar("_SchemaValueT")
_ItemT = _TypeVar("_ItemT")
_ValueT = _TypeVar("_ValueT")


def _utf8_size(value: str, field: str) -> int:
    try:
        return len(value.encode("utf-8"))
    except UnicodeEncodeError as error:
        raise ValueError(f"{field} must contain only Unicode scalar values") from error


def _description(value: object) -> str:
    if type(value) is not str:
        raise TypeError("description must be a str")
    size = _utf8_size(value, "description")
    if not value or not any(not character.isspace() for character in value):
        raise ValueError("description must contain a non-whitespace character")
    if len(value) > _DESCRIPTION_MAX_SCALARS or size > _DESCRIPTION_MAX_BYTES:
        raise ValueError("description exceeds ResourceLimitsV1")
    return value


def _json_kind(value: object) -> str:
    if type(value) is not str:
        raise TypeError("json_kind must be a str")
    if value not in _JSON_KINDS:
        allowed = ", ".join(repr(item) for item in sorted(_JSON_KINDS))
        raise ValueError(f"json_kind must be one of: {allowed}")
    return value


def _optional_size(value: object, field: str, maximum: int) -> int | None:
    if value is None:
        return None
    if type(value) is not int:
        raise TypeError(f"{field} must be an int or None")
    if value < 0:
        raise ValueError(f"{field} must be non-negative")
    if value > maximum:
        raise ValueError(f"{field} exceeds ResourceLimitsV1")
    return value


def _ordered_bounds(
    minimum: int | None,
    maximum: int | None,
    minimum_name: str,
    maximum_name: str,
) -> None:
    if minimum is not None and maximum is not None and minimum > maximum:
        raise ValueError(f"{minimum_name} must be less than or equal to {maximum_name}")


def _choice_sequence(value: object) -> tuple[object, ...]:
    if isinstance(value, (str, bytes, bytearray)) or not isinstance(value, _Sequence):
        raise TypeError("choices must be a finite sequence")
    choices = tuple(value)
    if not choices:
        raise ValueError("choices must not be empty")
    if len(choices) > _CHOICES_MAX_ITEMS:
        raise ValueError("choices exceeds ResourceLimitsV1")
    return choices


def _json_scalar(value: object) -> str:
    return _json.dumps(value, ensure_ascii=False, allow_nan=False, separators=(",", ":"))


class ValueRejected(ValueError):
    __slots__ = ()

    def __init__(self, message: str) -> None:
        if type(message) is not str:
            raise TypeError("message must be a str")
        super().__init__(message)


_SCHEMA_CALLBACK_METADATA = _weakref.WeakKeyDictionary()


class SchemaCallbackError(RuntimeError):
    __slots__ = ("__weakref__",)

    def __init__(self, message: str, *, phase: str, path: str) -> None:
        if phase not in {"render_prompt", "validate"}:
            raise ValueError("phase must be 'render_prompt' or 'validate'")
        if type(path) is not str:
            raise TypeError("path must be a str")
        super().__init__(message)
        _SCHEMA_CALLBACK_METADATA[self] = (phase, path)

    def __setattr__(self, name: str, value: object) -> None:
        raise AttributeError("SchemaCallbackError is immutable")

    @property
    def _phase(self) -> str:
        return _SCHEMA_CALLBACK_METADATA[self][0]

    @property
    def _path(self) -> str:
        return _SCHEMA_CALLBACK_METADATA[self][1]

    phase = _phase
    path = _path


class SchemaValue(_Generic[_SchemaValueT], _ABC):
    __slots__ = ("_description", "_json_kind")

    def __init__(self, *, description: str, json_kind: str) -> None:
        object.__setattr__(self, "_description", _description(description))
        object.__setattr__(self, "_json_kind", _json_kind(json_kind))

    @property
    def description(self) -> str:
        return self._description

    @property
    def json_kind(self) -> str:
        return self._json_kind

    @_abstractmethod
    def render_prompt(self) -> str:
        raise NotImplementedError

    @_abstractmethod
    def validate(self, value: _SchemaValueT, /) -> None | _Awaitable[None]:
        raise NotImplementedError


class _PythonValidationBridge:
    __slots__ = ("_closed", "_loop", "_queue", "_task")

    def __init__(self) -> None:
        self._closed = False
        self._loop = _asyncio.get_running_loop()
        self._queue = _asyncio.Queue()
        self._task = self._loop.create_task(self._run())

    async def _run(self) -> None:
        try:
            while True:
                validator, value, completion = await self._queue.get()
                if not completion.set_running_or_notify_cancel():
                    continue
                try:
                    outcome = validator.validate(value)
                    if _inspect.isawaitable(outcome):
                        outcome = await outcome
                except BaseException as error:
                    if self._closed and isinstance(error, _asyncio.CancelledError):
                        completion.cancel()
                        raise
                    completion.set_exception(error)
                else:
                    completion.set_result(outcome)
        except _asyncio.CancelledError:
            while True:
                try:
                    _, _, completion = self._queue.get_nowait()
                except _asyncio.QueueEmpty:
                    break
                completion.cancel()
            raise

    def _enqueue(self, validator: object, value: object, completion: object) -> None:
        if self._closed:
            completion.cancel()
        else:
            self._queue.put_nowait((validator, value, completion))

    def submit(self, validator: object, value: object) -> object:
        completion = _concurrent_futures.Future()
        if self._closed:
            completion.cancel()
        else:
            self._loop.call_soon_threadsafe(
                self._enqueue,
                validator,
                value,
                completion,
            )
        return completion

    def close(self) -> None:
        if not self._closed:
            self._closed = True
            self._loop.call_soon_threadsafe(self._task.cancel)


class _BuiltinValue(SchemaValue[_SchemaValueT], _Generic[_SchemaValueT]):
    __slots__ = ()

    def __setattr__(self, name: str, value: object) -> None:
        del name, value
        raise AttributeError("built-in schema values are immutable")


def _require_none_validation_result(result: object) -> None:
    if result is not None:
        raise TypeError("custom validate() must return None")


async def _await_child_validations(result: object, remaining: object) -> None:
    while True:
        result = await result
        _require_none_validation_result(result)
        for schema, value in remaining:
            result = schema.validate(value)
            if _inspect.isawaitable(result):
                break
            _require_none_validation_result(result)
        else:
            return


def _validate_children(children: object) -> None | _Awaitable[None]:
    remaining = iter(children)
    for schema, value in remaining:
        result = schema.validate(value)
        if _inspect.isawaitable(result):
            return _await_child_validations(result, remaining)
        _require_none_validation_result(result)
    return None


@_final
class StrValue(_BuiltinValue[str]):
    __slots__ = ("_min_length", "_max_length", "_choices")

    def __init__(
        self,
        *,
        description: str,
        min_length: int | None = None,
        max_length: int | None = None,
        choices: _Sequence[str] | None = None,
    ) -> None:
        minimum = _optional_size(min_length, "min_length", _STRING_MAX_SCALARS)
        maximum = _optional_size(max_length, "max_length", _STRING_MAX_SCALARS)
        _ordered_bounds(minimum, maximum, "min_length", "max_length")
        snapshot: tuple[str, ...] | None = None
        if choices is not None:
            raw_choices = _choice_sequence(choices)
            converted: list[str] = []
            seen: set[str] = set()
            for choice in raw_choices:
                if type(choice) is not str:
                    raise TypeError("string choices must contain only str values")
                _utf8_size(choice, "string choice")
                if len(choice) > _STRING_MAX_SCALARS or _utf8_size(choice, "string choice") > _STRING_MAX_BYTES:
                    raise ValueError("string choice exceeds ResourceLimitsV1")
                if minimum is not None and len(choice) < minimum:
                    raise ValueError("string choice violates min_length")
                if maximum is not None and len(choice) > maximum:
                    raise ValueError("string choice violates max_length")
                if choice in seen:
                    raise ValueError("choices must not contain duplicates")
                seen.add(choice)
                converted.append(choice)
            snapshot = tuple(converted)
        super().__init__(description=description, json_kind="string")
        object.__setattr__(self, "_min_length", minimum)
        object.__setattr__(self, "_max_length", maximum)
        object.__setattr__(self, "_choices", snapshot)

    def render_prompt(self) -> str:
        constraints = ["must be a JSON string"]
        if self._min_length is not None:
            constraints.append(f"minimum length {self._min_length}")
        if self._max_length is not None:
            constraints.append(f"maximum length {self._max_length}")
        if self._choices is not None:
            constraints.append(
                "one of [" + ", ".join(_json_scalar(choice) for choice in self._choices) + "]"
            )
        return "; ".join(constraints)

    def validate(self, value: str, /) -> None:
        if type(value) is not str:
            raise ValueRejected("value must be a string")
        try:
            encoded = _utf8_size(value, "value")
        except ValueError as error:
            raise ValueRejected("value must contain only Unicode scalar values") from error
        if len(value) > _STRING_MAX_SCALARS or encoded > _STRING_MAX_BYTES:
            raise ValueRejected("value exceeds the string resource limit")
        if self._min_length is not None and len(value) < self._min_length:
            raise ValueRejected("value is shorter than min_length")
        if self._max_length is not None and len(value) > self._max_length:
            raise ValueRejected("value is longer than max_length")
        if self._choices is not None and value not in self._choices:
            raise ValueRejected("value is not in choices")


def _optional_int64(value: object, field: str) -> int | None:
    if value is None:
        return None
    if type(value) is not int:
        raise TypeError(f"{field} must be an int or None")
    if not _INT64_MIN <= value <= _INT64_MAX:
        raise ValueError(f"{field} must fit signed int64")
    return value


@_final
class Int64Value(_BuiltinValue[int]):
    __slots__ = ("_min", "_max", "_choices")

    def __init__(
        self,
        *,
        description: str,
        min: int | None = None,
        max: int | None = None,
        choices: _Sequence[int] | None = None,
    ) -> None:
        minimum = _optional_int64(min, "min")
        maximum = _optional_int64(max, "max")
        _ordered_bounds(minimum, maximum, "min", "max")
        snapshot: tuple[int, ...] | None = None
        if choices is not None:
            raw_choices = _choice_sequence(choices)
            converted: list[int] = []
            seen: set[int] = set()
            for choice in raw_choices:
                canonical = _optional_int64(choice, "integer choice")
                if canonical is None:
                    raise TypeError("integer choice must be an int")
                if minimum is not None and canonical < minimum:
                    raise ValueError("integer choice violates min")
                if maximum is not None and canonical > maximum:
                    raise ValueError("integer choice violates max")
                if canonical in seen:
                    raise ValueError("choices must not contain duplicates")
                seen.add(canonical)
                converted.append(canonical)
            snapshot = tuple(converted)
        super().__init__(description=description, json_kind="int64")
        object.__setattr__(self, "_min", minimum)
        object.__setattr__(self, "_max", maximum)
        object.__setattr__(self, "_choices", snapshot)

    def render_prompt(self) -> str:
        constraints = ["must be a signed 64-bit JSON integer"]
        if self._min is not None or self._max is not None:
            constraints.append(f"inclusive range {self._min!r} to {self._max!r}")
        if self._choices is not None:
            constraints.append("one of [" + ", ".join(map(str, self._choices)) + "]")
        return "; ".join(constraints)

    def validate(self, value: int, /) -> None:
        if type(value) is not int or not _INT64_MIN <= value <= _INT64_MAX:
            raise ValueRejected("value must be a signed 64-bit integer")
        if self._min is not None and value < self._min:
            raise ValueRejected("value is below min")
        if self._max is not None and value > self._max:
            raise ValueRejected("value is above max")
        if self._choices is not None and value not in self._choices:
            raise ValueRejected("value is not in choices")


def _float64(value: object, field: str) -> float:
    if type(value) not in {int, float}:
        raise TypeError(f"{field} must be an int or float")
    try:
        canonical = float(value)
    except OverflowError as error:
        raise ValueError(f"{field} must fit finite float64") from error
    if not _math.isfinite(canonical):
        raise ValueError(f"{field} must be finite")
    return canonical


def _optional_float64(value: object, field: str) -> float | None:
    return None if value is None else _float64(value, field)


@_final
class Float64Value(_BuiltinValue[float]):
    __slots__ = ("_min", "_max", "_choices")

    def __init__(
        self,
        *,
        description: str,
        min: float | None = None,
        max: float | None = None,
        choices: _Sequence[float] | None = None,
    ) -> None:
        minimum = _optional_float64(min, "min")
        maximum = _optional_float64(max, "max")
        if minimum is not None and maximum is not None and minimum > maximum:
            raise ValueError("min must be less than or equal to max")
        snapshot: tuple[float, ...] | None = None
        if choices is not None:
            raw_choices = _choice_sequence(choices)
            converted: list[float] = []
            seen: set[float] = set()
            for choice in raw_choices:
                canonical = _float64(choice, "float choice")
                if minimum is not None and canonical < minimum:
                    raise ValueError("float choice violates min")
                if maximum is not None and canonical > maximum:
                    raise ValueError("float choice violates max")
                if canonical in seen:
                    raise ValueError("choices must not contain duplicates")
                seen.add(canonical)
                converted.append(canonical)
            snapshot = tuple(converted)
        super().__init__(description=description, json_kind="float64")
        object.__setattr__(self, "_min", minimum)
        object.__setattr__(self, "_max", maximum)
        object.__setattr__(self, "_choices", snapshot)

    def render_prompt(self) -> str:
        constraints = ["must be a finite JSON number representable as float64"]
        if self._min is not None or self._max is not None:
            constraints.append(f"inclusive range {self._min!r} to {self._max!r}")
        if self._choices is not None:
            constraints.append(
                "one of [" + ", ".join(_json_scalar(choice) for choice in self._choices) + "]"
            )
        return "; ".join(constraints)

    def validate(self, value: float, /) -> None:
        try:
            canonical = _float64(value, "value")
        except (TypeError, ValueError) as error:
            raise ValueRejected("value must be a finite float64 number") from error
        if self._min is not None and canonical < self._min:
            raise ValueRejected("value is below min")
        if self._max is not None and canonical > self._max:
            raise ValueRejected("value is above max")
        if self._choices is not None and canonical not in self._choices:
            raise ValueRejected("value is not in choices")


@_final
class BoolValue(_BuiltinValue[bool]):
    __slots__ = ("_choices",)

    def __init__(
        self,
        *,
        description: str,
        choices: _Sequence[bool] | None = None,
    ) -> None:
        snapshot: tuple[bool, ...] | None = None
        if choices is not None:
            raw_choices = _choice_sequence(choices)
            converted: list[bool] = []
            seen: set[bool] = set()
            for choice in raw_choices:
                if type(choice) is not bool:
                    raise TypeError("boolean choices must contain only bool values")
                if choice in seen:
                    raise ValueError("choices must not contain duplicates")
                seen.add(choice)
                converted.append(choice)
            snapshot = tuple(converted)
        super().__init__(description=description, json_kind="bool")
        object.__setattr__(self, "_choices", snapshot)

    def render_prompt(self) -> str:
        constraints = ["must be a JSON boolean"]
        if self._choices is not None:
            constraints.append(
                "one of [" + ", ".join(_json_scalar(choice) for choice in self._choices) + "]"
            )
        return "; ".join(constraints)

    def validate(self, value: bool, /) -> None:
        if type(value) is not bool:
            raise ValueRejected("value must be a boolean")
        if self._choices is not None and value not in self._choices:
            raise ValueRejected("value is not in choices")


@_final
class ListValue(_BuiltinValue[list[_ItemT]], _Generic[_ItemT]):
    __slots__ = ("_item", "_min_items", "_max_items")

    def __init__(
        self,
        item: SchemaValue[_ItemT],
        *,
        description: str,
        min_items: int | None = None,
        max_items: int | None = None,
    ) -> None:
        if not isinstance(item, SchemaValue):
            raise TypeError("item must be a SchemaValue")
        minimum = _optional_size(min_items, "min_items", _LIST_MAX_ITEMS)
        maximum = _optional_size(max_items, "max_items", _LIST_MAX_ITEMS)
        _ordered_bounds(minimum, maximum, "min_items", "max_items")
        super().__init__(description=description, json_kind="array")
        object.__setattr__(self, "_item", item)
        object.__setattr__(self, "_min_items", minimum)
        object.__setattr__(self, "_max_items", maximum)

    def render_prompt(self) -> str:
        constraints = ["must be a JSON array", f"items {self._item.render_prompt()}"]
        if self._min_items is not None:
            constraints.append(f"minimum items {self._min_items}")
        if self._max_items is not None:
            constraints.append(f"maximum items {self._max_items}")
        return "; ".join(constraints)

    def validate(self, value: list[_ItemT], /) -> None | _Awaitable[None]:
        if type(value) is not list:
            raise ValueRejected("value must be an array")
        if len(value) > _LIST_MAX_ITEMS:
            raise ValueRejected("value exceeds the array resource limit")
        if self._min_items is not None and len(value) < self._min_items:
            raise ValueRejected("value has fewer than min_items")
        if self._max_items is not None and len(value) > self._max_items:
            raise ValueRejected("value has more than max_items")
        return _validate_children((self._item, item) for item in value)


@_final
class Field(_Generic[_ValueT]):
    __slots__ = ("_inner", "_required")

    def __init__(self, inner: SchemaValue[_ValueT], *, required: bool) -> None:
        if not isinstance(inner, SchemaValue):
            raise TypeError("inner must be a SchemaValue")
        if type(required) is not bool:
            raise TypeError("required must be a bool")
        object.__setattr__(self, "_inner", inner)
        object.__setattr__(self, "_required", required)

    def __setattr__(self, name: str, value: object) -> None:
        del name, value
        raise AttributeError("Field is immutable")


def _field_snapshot(fields: object) -> tuple[tuple[str, SchemaValue[_Any], bool], ...]:
    if type(fields) is not dict:
        raise TypeError("fields must be a dict")
    snapshot: list[tuple[str, SchemaValue[_Any], bool]] = []
    for name, spec in fields.items():
        if type(name) is not str:
            raise TypeError("field names must be str")
        _utf8_size(name, "field name")
        if type(spec) is Field:
            snapshot.append((name, spec._inner, spec._required))
        elif isinstance(spec, Field):
            raise TypeError("Field subclasses are not supported")
        elif isinstance(spec, SchemaValue):
            snapshot.append((name, spec, True))
        else:
            raise TypeError("field values must be SchemaValue or Field")
    return tuple(snapshot)


@_final
class ObjectValue(_BuiltinValue[dict[str, _Any]]):
    __slots__ = ("_fields",)

    def __init__(self, *, description: str, fields: dict[str, _Any]) -> None:
        snapshot = _field_snapshot(fields)
        super().__init__(description=description, json_kind="object")
        object.__setattr__(self, "_fields", snapshot)

    def render_prompt(self) -> str:
        rendered = []
        for name, value, required in self._fields:
            marker = "" if required else "?"
            rendered.append(f"{_json_scalar(name)}{marker}: {value.render_prompt()}")
        return "must be a fixed JSON object {" + ", ".join(rendered) + "}"

    def validate(self, value: dict[str, _Any], /) -> None | _Awaitable[None]:
        if type(value) is not dict:
            raise ValueRejected("value must be an object")
        expected = {name for name, _, _ in self._fields}
        if any(type(name) is not str for name in value):
            raise ValueRejected("object keys must be strings")
        if set(value) - expected:
            raise ValueRejected("object contains extra fields")
        for name, schema, required in self._fields:
            if name not in value:
                if required:
                    raise ValueRejected("object is missing a required field")
                continue
        return _validate_children(
            (schema, value[name])
            for name, schema, _ in self._fields
            if name in value
        )


@_final
class NullableValue(_BuiltinValue[_ValueT | None], _Generic[_ValueT]):
    __slots__ = ("_inner",)

    def __init__(self, inner: SchemaValue[_ValueT]) -> None:
        if not isinstance(inner, SchemaValue):
            raise TypeError("inner must be a SchemaValue")
        super().__init__(description=inner.description, json_kind=inner.json_kind)
        object.__setattr__(self, "_inner", inner)

    def render_prompt(self) -> str:
        return self._inner.render_prompt() + "; null is also allowed"

    def validate(self, value: _ValueT | None, /) -> None | _Awaitable[None]:
        if value is None:
            return None
        return self._inner.validate(value)


FieldSpec = SchemaValue[_Any] | Field[_Any]

__all__ = [
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
"####;

const SCRIPT_MAX_BYTES: usize = 256 * 1024;
const SCHEMA_MAX_DEPTH: usize = 32;
const SCHEMA_MAX_NODES: usize = 1_024;
const SCHEMA_MAX_FIELDS: usize = 512;
const SCHEMA_MAX_CHOICES: usize = 4_096;
const SCHEMA_MAX_CHOICE_BYTES: usize = 256 * 1024;
const PROMPT_MAX_BYTES: usize = 1024 * 1024;
const RESULT_MAX_DEPTH: usize = 32;
const RESULT_MAX_NODES: usize = 65_536;
const RESULT_STRING_MAX_SCALARS: usize = 1_048_576;
const RESULT_STRING_MAX_BYTES: usize = 4 * 1024 * 1024;
const BIG_INT_LEAF_DIGITS: usize = 38;
const RESULT_LIST_MAX_ITEMS: usize = 10_000;
const MAX_VALIDATION_ISSUES: usize = 16;
const CUSTOM_PROMPT_MAX_BYTES: usize = 16 * 1024;
const DESCRIPTION_MAX_SCALARS: usize = 1_024;
const DESCRIPTION_MAX_BYTES: usize = 4 * 1024;
const SCALAR_CHOICES_MAX_ITEMS: usize = 256;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SchemaValidationMode {
    NativeOnly,
    Hybrid,
}

#[derive(Clone, Debug)]
enum ValueContract {
    String {
        description: String,
        min_length: Option<usize>,
        max_length: usize,
        choices: Option<Vec<String>>,
    },
    Int64 {
        description: String,
        min: Option<i64>,
        max: Option<i64>,
        choices: Option<Vec<i64>>,
    },
    Float64 {
        description: String,
        min: Option<f64>,
        max: Option<f64>,
        choices: Option<Vec<f64>>,
    },
    Bool {
        description: String,
        choices: Option<Vec<bool>>,
    },
    List {
        description: String,
        item: Box<ValueContract>,
        min_items: Option<usize>,
        max_items: usize,
    },
    Object {
        description: String,
        contract: ObjectContract,
    },
    Nullable(Box<ValueContract>),
    Custom {
        description: String,
        json_kind: JsonKind,
        prompt_fragment: String,
        validator_id: usize,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum JsonKind {
    String,
    Int64,
    Float64,
    Bool,
    Array,
    Object,
}

impl JsonKind {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "string" => Some(Self::String),
            "int64" => Some(Self::Int64),
            "float64" => Some(Self::Float64),
            "bool" => Some(Self::Bool),
            "array" => Some(Self::Array),
            "object" => Some(Self::Object),
            _ => None,
        }
    }

    const fn name(self) -> &'static str {
        match self {
            Self::String => "string",
            Self::Int64 => "int64",
            Self::Float64 => "float64",
            Self::Bool => "bool",
            Self::Array => "array",
            Self::Object => "object",
        }
    }
}

impl ValueContract {
    fn description(&self) -> &str {
        match self {
            Self::String { description, .. }
            | Self::Int64 { description, .. }
            | Self::Float64 { description, .. }
            | Self::Bool { description, .. }
            | Self::List { description, .. }
            | Self::Object { description, .. }
            | Self::Custom { description, .. } => description,
            Self::Nullable(inner) => inner.description(),
        }
    }

    fn json_kind(&self) -> JsonKind {
        match self {
            Self::String { .. } => JsonKind::String,
            Self::Int64 { .. } => JsonKind::Int64,
            Self::Float64 { .. } => JsonKind::Float64,
            Self::Bool { .. } => JsonKind::Bool,
            Self::List { .. } => JsonKind::Array,
            Self::Object { .. } => JsonKind::Object,
            Self::Nullable(inner) => inner.json_kind(),
            Self::Custom { json_kind, .. } => *json_kind,
        }
    }
}

#[derive(Clone, Debug)]
struct ObjectContract {
    fields: Vec<FieldContract>,
}

#[derive(Clone, Debug)]
struct FieldContract {
    name: String,
    required: bool,
    value: ValueContract,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum ValidatedActValue {
    String(String),
    Int64(i64),
    UInt64(u64),
    BigInt(String),
    Float64(f64),
    Bool(bool),
    Null,
    List(Vec<ValidatedActValue>),
    Object(Vec<(String, ValidatedActValue)>),
}

impl ValidatedActValue {
    pub(crate) fn into_py(self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        match self {
            Self::String(value) => value.into_py_any(py),
            Self::Int64(value) => value.into_py_any(py),
            Self::UInt64(value) => value.into_py_any(py),
            Self::BigInt(value) => arbitrary_precision_int(py, &value),
            Self::Float64(value) => value.into_py_any(py),
            Self::Bool(value) => value.into_py_any(py),
            Self::Null => Ok(py.None()),
            Self::List(values) => {
                let values = values
                    .into_iter()
                    .map(|value| value.into_py(py))
                    .collect::<PyResult<Vec<_>>>()?;
                Ok(PyList::new(py, values)?.into_any().unbind())
            }
            Self::Object(fields) => {
                let value = PyDict::new(py);
                for (name, field) in fields {
                    value.set_item(name, field.into_py(py)?)?;
                }
                Ok(value.into_any().unbind())
            }
        }
    }
}

fn arbitrary_precision_int(py: Python<'_>, encoded: &str) -> PyResult<Py<PyAny>> {
    let (negative, digits) = encoded
        .strip_prefix('-')
        .map_or((false, encoded), |digits| (true, digits));
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(PyValueError::new_err(
            "invalid arbitrary-size integer token",
        ));
    }

    let ten = 10_u8.into_py_any(py)?;
    let mut powers = HashMap::new();
    let value = decimal_digits_into_py(py, digits, &ten, &mut powers)?;
    if negative {
        Ok(value.bind(py).neg()?.unbind())
    } else {
        Ok(value)
    }
}

fn decimal_digits_into_py(
    py: Python<'_>,
    digits: &str,
    ten: &Py<PyAny>,
    powers: &mut HashMap<usize, Py<PyAny>>,
) -> PyResult<Py<PyAny>> {
    if digits.len() <= BIG_INT_LEAF_DIGITS {
        let value = digits
            .parse::<u128>()
            .map_err(|_| PyValueError::new_err("invalid arbitrary-size integer token"))?;
        return value.into_py_any(py);
    }

    let midpoint = digits.len() / 2;
    let (left_digits, right_digits) = digits.split_at(midpoint);
    let left = decimal_digits_into_py(py, left_digits, ten, powers)?;
    let right = decimal_digits_into_py(py, right_digits, ten, powers)?;
    let scale = match powers.get(&right_digits.len()) {
        Some(scale) => scale.clone_ref(py),
        None => {
            let scale = ten.bind(py).pow(right_digits.len(), py.None())?.unbind();
            powers.insert(right_digits.len(), scale.clone_ref(py));
            scale
        }
    };
    Ok(left
        .bind(py)
        .mul(scale.bind(py))?
        .add(right.bind(py))?
        .unbind())
}

pub(crate) fn defensive_python_copy(
    value: &ValidatedActValue,
    py: Python<'_>,
) -> PyResult<Py<PyAny>> {
    value.clone().into_py(py)
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct CustomValidationJob {
    pub(crate) validator_id: usize,
    pub(crate) path: String,
    pub(crate) value: ValidatedActValue,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ValidationIssue {
    pub(crate) path: String,
    pub(crate) code: &'static str,
    pub(crate) message: String,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum NativeValidationOutcome {
    Valid {
        value: ValidatedActValue,
        custom_jobs: Vec<CustomValidationJob>,
    },
    Invalid {
        issues: Vec<ValidationIssue>,
        truncated: bool,
    },
}

#[derive(Debug)]
pub(crate) struct CompiledActSchema {
    root: ObjectContract,
    validation_mode: SchemaValidationMode,
    custom_validators: Vec<Py<PyAny>>,
}

impl CompiledActSchema {
    pub(crate) const fn validation_mode(&self) -> SchemaValidationMode {
        self.validation_mode
    }

    pub(crate) fn render_prompt(&self, script: &str) -> PyResult<String> {
        if script.len() > SCRIPT_MAX_BYTES {
            return Err(PyValueError::new_err("script exceeds ResourceLimitsV1"));
        }
        let encoded_script =
            serde_json::to_string(script).expect("a Rust string always has a JSON representation");
        let mut contract = String::new();
        render_object(&self.root, 0, &mut contract);
        let prompt = format!(
            "TROUPE_ACT_V1\nSCRIPT_JSON\n{encoded_script}\nRESULT_CONTRACT\n{contract}\n\
             Submit exactly one accepted result through troupe_submit_result.\n\
             Pass value as a JSON object, not a JSON-encoded string.\n\
             Extra fields are forbidden. Correct validation errors within this same turn.\n\
             Assistant text is not a result channel."
        );
        if prompt.len() > PROMPT_MAX_BYTES {
            return Err(PyValueError::new_err(
                "rendered prompt exceeds ResourceLimitsV1",
            ));
        }
        Ok(prompt)
    }

    pub(crate) fn validate(&self, value: &JsonValue) -> NativeValidationOutcome {
        let mut nodes = 0;
        if let Some(issue) = check_result_resources(value, "", 1, &mut nodes) {
            return NativeValidationOutcome::Invalid {
                issues: vec![issue],
                truncated: false,
            };
        }
        let mut issues = IssueCollector::default();
        let mut custom_jobs = Vec::new();
        let validated = validate_object(&self.root, value, "", &mut issues, &mut custom_jobs);
        if issues.items.is_empty() {
            NativeValidationOutcome::Valid {
                value: validated.expect("valid root object materializes a value"),
                custom_jobs,
            }
        } else {
            NativeValidationOutcome::Invalid {
                issues: issues.items,
                truncated: issues.truncated,
            }
        }
    }

    pub(crate) fn custom_validator(&self, id: usize) -> Option<&Py<PyAny>> {
        self.custom_validators.get(id)
    }

    pub(crate) fn traverse(&self, visit: &PyVisit<'_>) -> Result<(), PyTraverseError> {
        for validator in &self.custom_validators {
            visit.call(validator)?;
        }
        Ok(())
    }
}

pub(crate) fn extract_script(value: &Bound<'_, PyAny>) -> PyResult<String> {
    let value = value
        .cast::<PyString>()
        .map_err(|_| PyTypeError::new_err("script must be a str"))?;
    let value = value
        .to_str()
        .map_err(|_| PyValueError::new_err("script must contain only Unicode scalar values"))?
        .to_owned();
    if value.len() > SCRIPT_MAX_BYTES {
        return Err(PyValueError::new_err("script exceeds ResourceLimitsV1"));
    }
    Ok(value)
}

struct SchemaTypes<'py> {
    schema_value: Bound<'py, PyType>,
    str_value: Bound<'py, PyType>,
    int64_value: Bound<'py, PyType>,
    float64_value: Bound<'py, PyType>,
    bool_value: Bound<'py, PyType>,
    list_value: Bound<'py, PyType>,
    object_value: Bound<'py, PyType>,
    nullable_value: Bound<'py, PyType>,
    field: Bound<'py, PyType>,
}

impl<'py> SchemaTypes<'py> {
    fn load(py: Python<'py>) -> PyResult<Self> {
        let module = py.import("troupe.act_schema")?;
        Ok(Self {
            schema_value: module.getattr("SchemaValue")?.cast_into()?,
            str_value: module.getattr("StrValue")?.cast_into()?,
            int64_value: module.getattr("Int64Value")?.cast_into()?,
            float64_value: module.getattr("Float64Value")?.cast_into()?,
            bool_value: module.getattr("BoolValue")?.cast_into()?,
            list_value: module.getattr("ListValue")?.cast_into()?,
            object_value: module.getattr("ObjectValue")?.cast_into()?,
            nullable_value: module.getattr("NullableValue")?.cast_into()?,
            field: module.getattr("Field")?.cast_into()?,
        })
    }

    fn is_builtin_instance(&self, value: &Bound<'_, PyAny>) -> PyResult<bool> {
        for class in [
            &self.str_value,
            &self.int64_value,
            &self.float64_value,
            &self.bool_value,
            &self.list_value,
            &self.object_value,
            &self.nullable_value,
        ] {
            if value.is_instance(class)? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn custom_base_metadata(&self, value: &Bound<'_, PyAny>) -> PyResult<(String, JsonKind)> {
        let base_slot = |name: &str| -> PyResult<Bound<'_, PyAny>> {
            self.schema_value
                .getattr(name)?
                .call_method1("__get__", (value, value.get_type()))
                .map_err(|_| {
                    PyTypeError::new_err("custom SchemaValue must call SchemaValue.__init__()")
                })
        };
        let description = extract_custom_description(&base_slot("_description")?)?;
        let json_kind = extract_exact_string(
            &base_slot("_json_kind")?,
            "custom json_kind must be an exact str",
        )?;
        let json_kind = JsonKind::parse(&json_kind)
            .ok_or_else(|| PyValueError::new_err("custom json_kind is invalid"))?;
        Ok((description, json_kind))
    }
}

#[derive(Default)]
struct CompileStats {
    nodes: usize,
    fields: usize,
    choices: usize,
    choice_bytes: usize,
}

#[derive(Clone)]
struct CustomBinding {
    validator_id: usize,
    description: String,
    json_kind: JsonKind,
    prompt_fragment: String,
}

struct Compiler<'py> {
    types: SchemaTypes<'py>,
    stats: CompileStats,
    active: HashSet<usize>,
    custom_memo: HashMap<usize, CustomBinding>,
    custom_validators: Vec<Py<PyAny>>,
}

pub(crate) fn compile_act_schema(schema: &Bound<'_, PyAny>) -> PyResult<CompiledActSchema> {
    let py = schema.py();
    if !schema.get_type().is(py.get_type::<PyDict>()) {
        return Err(PyTypeError::new_err("output_schema must be an exact dict"));
    }
    let schema = schema.cast::<PyDict>()?;
    let mut compiler = Compiler {
        types: SchemaTypes::load(py)?,
        stats: CompileStats {
            nodes: 1,
            ..CompileStats::default()
        },
        active: HashSet::new(),
        custom_memo: HashMap::new(),
        custom_validators: Vec::new(),
    };
    let root = compiler.compile_object_fields(schema.iter(), 1, "")?;
    if compiler.stats.nodes > SCHEMA_MAX_NODES {
        return Err(PyValueError::new_err(
            "compiled schema exceeds the node limit",
        ));
    }
    if compiler.stats.fields > SCHEMA_MAX_FIELDS {
        return Err(PyValueError::new_err(
            "compiled schema exceeds the field limit",
        ));
    }
    if compiler.stats.choices > SCHEMA_MAX_CHOICES
        || compiler.stats.choice_bytes > SCHEMA_MAX_CHOICE_BYTES
    {
        return Err(PyValueError::new_err(
            "compiled schema choices exceed ResourceLimitsV1",
        ));
    }
    let validation_mode = if compiler.custom_validators.is_empty() {
        SchemaValidationMode::NativeOnly
    } else {
        SchemaValidationMode::Hybrid
    };
    Ok(CompiledActSchema {
        root,
        validation_mode,
        custom_validators: compiler.custom_validators,
    })
}

impl Compiler<'_> {
    fn compile_object_fields<'a>(
        &mut self,
        fields: impl Iterator<Item = (Bound<'a, PyAny>, Bound<'a, PyAny>)>,
        depth: usize,
        path: &str,
    ) -> PyResult<ObjectContract> {
        let mut compiled = Vec::new();
        for (name, spec) in fields {
            let name = name
                .cast::<PyString>()
                .map_err(|_| PyTypeError::new_err("schema field names must be exact str"))?;
            if !name.get_type().is(name.py().get_type::<PyString>()) {
                return Err(PyTypeError::new_err("schema field names must be exact str"));
            }
            let name = name
                .to_str()
                .map_err(|_| {
                    PyValueError::new_err(
                        "schema field names must contain only Unicode scalar values",
                    )
                })?
                .to_owned();
            let field_path = join_path(path, &name);
            let (value, required) = if spec.get_type().is(&self.types.field) {
                (
                    spec.getattr("_inner")?,
                    extract_exact_bool(&spec.getattr("_required")?, "required")?,
                )
            } else {
                if spec.is_instance(&self.types.field)? {
                    return Err(PyTypeError::new_err("Field subclasses are not supported"));
                }
                (spec, true)
            };
            self.stats.fields += 1;
            compiled.push(FieldContract {
                name,
                required,
                value: self.compile_value(&value, depth + 1, &field_path)?,
            });
        }
        Ok(ObjectContract { fields: compiled })
    }

    fn compile_value(
        &mut self,
        value: &Bound<'_, PyAny>,
        depth: usize,
        path: &str,
    ) -> PyResult<ValueContract> {
        if depth > SCHEMA_MAX_DEPTH {
            return Err(PyValueError::new_err(
                "compiled schema exceeds the depth limit",
            ));
        }
        self.stats.nodes += 1;
        if self.stats.nodes > SCHEMA_MAX_NODES {
            return Err(PyValueError::new_err(
                "compiled schema exceeds the node limit",
            ));
        }
        let identity = value.as_ptr() as usize;
        if !self.active.insert(identity) {
            return Err(PyValueError::new_err(format!(
                "schema graph contains a cycle at {path}"
            )));
        }
        let result = self.compile_value_inner(value, depth, path);
        self.active.remove(&identity);
        result
    }

    fn compile_value_inner(
        &mut self,
        value: &Bound<'_, PyAny>,
        depth: usize,
        path: &str,
    ) -> PyResult<ValueContract> {
        if value.get_type().is(&self.types.str_value) {
            let description = extract_builtin_metadata(value, JsonKind::String)?;
            let min_length = extract_exact_optional_usize(
                &value.getattr("_min_length")?,
                "min_length",
                RESULT_STRING_MAX_SCALARS,
            )?;
            let max_length = extract_exact_optional_usize(
                &value.getattr("_max_length")?,
                "max_length",
                RESULT_STRING_MAX_SCALARS,
            )?
            .unwrap_or(RESULT_STRING_MAX_SCALARS);
            if min_length.is_some_and(|minimum| minimum > max_length) {
                return Err(PyValueError::new_err(
                    "min_length snapshot must not exceed max_length",
                ));
            }
            let choices =
                extract_string_choices(&value.getattr("_choices")?, min_length, max_length)?;
            self.record_choices(&choices, |choice| serde_json::to_vec(choice).unwrap().len())?;
            return Ok(ValueContract::String {
                description,
                min_length,
                max_length,
                choices,
            });
        }
        if value.get_type().is(&self.types.int64_value) {
            let description = extract_builtin_metadata(value, JsonKind::Int64)?;
            let minimum = extract_exact_optional_i64(&value.getattr("_min")?, "min")?;
            let maximum = extract_exact_optional_i64(&value.getattr("_max")?, "max")?;
            if minimum
                .zip(maximum)
                .is_some_and(|(minimum, maximum)| minimum > maximum)
            {
                return Err(PyValueError::new_err("min snapshot must not exceed max"));
            }
            let choices = extract_int64_choices(&value.getattr("_choices")?, minimum, maximum)?;
            self.record_choices(&choices, |choice| choice.to_string().len())?;
            return Ok(ValueContract::Int64 {
                description,
                min: minimum,
                max: maximum,
                choices,
            });
        }
        if value.get_type().is(&self.types.float64_value) {
            let description = extract_builtin_metadata(value, JsonKind::Float64)?;
            let minimum = extract_exact_optional_f64(&value.getattr("_min")?, "min")?;
            let maximum = extract_exact_optional_f64(&value.getattr("_max")?, "max")?;
            if minimum
                .zip(maximum)
                .is_some_and(|(minimum, maximum)| minimum > maximum)
            {
                return Err(PyValueError::new_err("min snapshot must not exceed max"));
            }
            let choices = extract_float64_choices(&value.getattr("_choices")?, minimum, maximum)?;
            self.record_choices(&choices, |choice| serde_json::to_vec(choice).unwrap().len())?;
            return Ok(ValueContract::Float64 {
                description,
                min: minimum,
                max: maximum,
                choices,
            });
        }
        if value.get_type().is(&self.types.bool_value) {
            let description = extract_builtin_metadata(value, JsonKind::Bool)?;
            let choices = extract_bool_choices(&value.getattr("_choices")?)?;
            self.record_choices(&choices, |choice| usize::from(!*choice) + 4)?;
            return Ok(ValueContract::Bool {
                description,
                choices,
            });
        }
        if value.get_type().is(&self.types.list_value) {
            let description = extract_builtin_metadata(value, JsonKind::Array)?;
            let min_items = extract_exact_optional_usize(
                &value.getattr("_min_items")?,
                "min_items",
                RESULT_LIST_MAX_ITEMS,
            )?;
            let max_items = extract_exact_optional_usize(
                &value.getattr("_max_items")?,
                "max_items",
                RESULT_LIST_MAX_ITEMS,
            )?
            .unwrap_or(RESULT_LIST_MAX_ITEMS);
            if min_items.is_some_and(|minimum| minimum > max_items) {
                return Err(PyValueError::new_err(
                    "min_items snapshot must not exceed max_items",
                ));
            }
            return Ok(ValueContract::List {
                description,
                item: Box::new(self.compile_value(
                    &value.getattr("_item")?,
                    depth + 1,
                    &format!("{path}/*"),
                )?),
                min_items,
                max_items,
            });
        }
        if value.get_type().is(&self.types.object_value) {
            let description = extract_builtin_metadata(value, JsonKind::Object)?;
            let fields_value = value.getattr("_fields")?;
            if !fields_value
                .get_type()
                .is(fields_value.py().get_type::<PyTuple>())
            {
                return Err(PyTypeError::new_err(
                    "object fields snapshot must be an exact tuple",
                ));
            }
            let fields = fields_value.cast_into::<PyTuple>()?;
            let mut entries = Vec::with_capacity(fields.len());
            for field in fields.iter() {
                if !field.get_type().is(field.py().get_type::<PyTuple>()) {
                    return Err(PyTypeError::new_err(
                        "object field snapshot must be an exact tuple",
                    ));
                }
                let field = field.cast_into::<PyTuple>()?;
                if field.len() != 3 {
                    return Err(PyValueError::new_err(
                        "object field snapshot must contain name, value, and required",
                    ));
                }
                entries.push((field.get_item(0)?, field.get_item(1)?, field.get_item(2)?));
            }
            let mut compiled = Vec::with_capacity(entries.len());
            let mut names = HashSet::with_capacity(entries.len());
            for (name, field_value, required) in entries {
                let name = extract_exact_string(&name, "object field name must be an exact str")?;
                if !names.insert(name.clone()) {
                    return Err(PyValueError::new_err(
                        "object fields snapshot must not contain duplicate names",
                    ));
                }
                let field_path = join_path(path, &name);
                self.stats.fields += 1;
                compiled.push(FieldContract {
                    name,
                    required: extract_exact_bool(&required, "required")?,
                    value: self.compile_value(&field_value, depth + 1, &field_path)?,
                });
            }
            return Ok(ValueContract::Object {
                description,
                contract: ObjectContract { fields: compiled },
            });
        }
        if value.get_type().is(&self.types.nullable_value) {
            let description = extract_builtin_description(value)?;
            let json_kind = extract_builtin_json_kind(value)?;
            let inner = self.compile_value(&value.getattr("_inner")?, depth + 1, path)?;
            if description != inner.description() || json_kind != inner.json_kind() {
                return Err(PyValueError::new_err(
                    "NullableValue metadata snapshot must match its inner value",
                ));
            }
            return Ok(ValueContract::Nullable(Box::new(inner)));
        }
        if self.types.is_builtin_instance(value)? {
            return Err(PyTypeError::new_err(
                "concrete built-in SchemaValue subclasses are not supported",
            ));
        }
        if value.is_instance(&self.types.schema_value)? {
            let identity = value.as_ptr() as usize;
            let binding = match self.custom_memo.get(&identity) {
                Some(binding) => binding.clone(),
                None => {
                    let (description, json_kind) = self.types.custom_base_metadata(value)?;
                    let rendered = value.call_method0("render_prompt").map_err(|cause| {
                        schema_callback_error(value.py(), "render_prompt", path, cause)
                    })?;
                    if !rendered.get_type().is(rendered.py().get_type::<PyString>()) {
                        return Err(schema_callback_error(
                            value.py(),
                            "render_prompt",
                            path,
                            PyTypeError::new_err("custom render_prompt() must return an exact str"),
                        ));
                    }
                    let rendered = rendered
                        .cast::<PyString>()?
                        .to_str()
                        .map_err(|cause| {
                            schema_callback_error(value.py(), "render_prompt", path, cause)
                        })?
                        .to_owned();
                    if rendered.is_empty() || rendered.chars().all(char::is_whitespace) {
                        return Err(schema_callback_error(
                            value.py(),
                            "render_prompt",
                            path,
                            PyValueError::new_err(
                                "custom render_prompt() must return a non-blank fragment",
                            ),
                        ));
                    }
                    if rendered.len() > CUSTOM_PROMPT_MAX_BYTES {
                        return Err(schema_callback_error(
                            value.py(),
                            "render_prompt",
                            path,
                            PyValueError::new_err(
                                "custom render_prompt() exceeds ResourceLimitsV1",
                            ),
                        ));
                    }
                    let validator_id = self.custom_validators.len();
                    self.custom_validators.push(value.clone().unbind());
                    let binding = CustomBinding {
                        validator_id,
                        description,
                        json_kind,
                        prompt_fragment: rendered,
                    };
                    self.custom_memo.insert(identity, binding.clone());
                    binding
                }
            };
            return Ok(ValueContract::Custom {
                description: binding.description,
                json_kind: binding.json_kind,
                prompt_fragment: binding.prompt_fragment,
                validator_id: binding.validator_id,
            });
        }
        Err(PyTypeError::new_err(format!(
            "schema field {path} must be a SchemaValue or Field"
        )))
    }

    fn record_choices<T>(
        &mut self,
        choices: &Option<Vec<T>>,
        encoded_len: impl Fn(&T) -> usize,
    ) -> PyResult<()> {
        if let Some(choices) = choices {
            self.stats.choices += choices.len();
            for choice in choices {
                self.stats.choice_bytes += encoded_len(choice);
            }
            if self.stats.choices > SCHEMA_MAX_CHOICES
                || self.stats.choice_bytes > SCHEMA_MAX_CHOICE_BYTES
            {
                return Err(PyValueError::new_err(
                    "compiled schema choices exceed ResourceLimitsV1",
                ));
            }
        }
        Ok(())
    }
}

fn extract_exact_string(value: &Bound<'_, PyAny>, message: &'static str) -> PyResult<String> {
    if !value.get_type().is(value.py().get_type::<PyString>()) {
        return Err(PyTypeError::new_err(message));
    }
    value
        .cast::<PyString>()?
        .to_str()
        .map(str::to_owned)
        .map_err(|_| PyValueError::new_err("schema string must contain only Unicode scalar values"))
}

fn extract_custom_description(value: &Bound<'_, PyAny>) -> PyResult<String> {
    let description = extract_exact_string(value, "description must be an exact str")?;
    if description.is_empty() || description.chars().all(char::is_whitespace) {
        return Err(PyValueError::new_err(
            "description must contain a non-whitespace character",
        ));
    }
    if description.chars().count() > DESCRIPTION_MAX_SCALARS
        || description.len() > DESCRIPTION_MAX_BYTES
    {
        return Err(PyValueError::new_err(
            "description exceeds ResourceLimitsV1",
        ));
    }
    Ok(description)
}

fn extract_builtin_description(value: &Bound<'_, PyAny>) -> PyResult<String> {
    extract_custom_description(&value.getattr("_description")?)
}

fn extract_builtin_json_kind(value: &Bound<'_, PyAny>) -> PyResult<JsonKind> {
    let json_kind = extract_exact_string(
        &value.getattr("_json_kind")?,
        "built-in json_kind snapshot must be an exact str",
    )?;
    JsonKind::parse(&json_kind)
        .ok_or_else(|| PyValueError::new_err("built-in json_kind snapshot is invalid"))
}

fn extract_builtin_metadata(value: &Bound<'_, PyAny>, expected_kind: JsonKind) -> PyResult<String> {
    let description = extract_builtin_description(value)?;
    if extract_builtin_json_kind(value)? != expected_kind {
        return Err(PyValueError::new_err(
            "built-in json_kind snapshot does not match its descriptor type",
        ));
    }
    Ok(description)
}

fn extract_exact_optional_usize(
    value: &Bound<'_, PyAny>,
    name: &'static str,
    maximum: usize,
) -> PyResult<Option<usize>> {
    if value.is_none() {
        return Ok(None);
    }
    if !value.get_type().is(value.py().get_type::<PyInt>()) {
        return Err(PyTypeError::new_err(format!(
            "{name} snapshot must be an exact int or None"
        )));
    }
    let value = value.extract::<usize>()?;
    if value > maximum {
        return Err(PyValueError::new_err(format!(
            "{name} snapshot exceeds ResourceLimitsV1"
        )));
    }
    Ok(Some(value))
}

fn extract_exact_optional_i64(
    value: &Bound<'_, PyAny>,
    name: &'static str,
) -> PyResult<Option<i64>> {
    if value.is_none() {
        return Ok(None);
    }
    if !value.get_type().is(value.py().get_type::<PyInt>()) {
        return Err(PyTypeError::new_err(format!(
            "{name} snapshot must be an exact int or None"
        )));
    }
    Ok(Some(value.extract::<i64>()?))
}

fn extract_exact_optional_f64(
    value: &Bound<'_, PyAny>,
    name: &'static str,
) -> PyResult<Option<f64>> {
    if value.is_none() {
        return Ok(None);
    }
    if !value.get_type().is(value.py().get_type::<PyFloat>()) {
        return Err(PyTypeError::new_err(format!(
            "{name} snapshot must be an exact float or None"
        )));
    }
    let value = value.extract::<f64>()?;
    if !value.is_finite() {
        return Err(PyValueError::new_err(format!(
            "{name} snapshot must be finite"
        )));
    }
    Ok(Some(value))
}

fn extract_exact_bool(value: &Bound<'_, PyAny>, name: &'static str) -> PyResult<bool> {
    if !value.get_type().is(value.py().get_type::<PyBool>()) {
        return Err(PyTypeError::new_err(format!(
            "{name} snapshot must be an exact bool"
        )));
    }
    value.extract()
}

fn extract_choice_tuple<'py>(value: &Bound<'py, PyAny>) -> PyResult<Option<Bound<'py, PyTuple>>> {
    if value.is_none() {
        return Ok(None);
    }
    if !value.get_type().is(value.py().get_type::<PyTuple>()) {
        return Err(PyTypeError::new_err(
            "choices snapshot must be an exact tuple or None",
        ));
    }
    let choices = value.clone().cast_into::<PyTuple>()?;
    if choices.is_empty() {
        return Err(PyValueError::new_err("choices snapshot must not be empty"));
    }
    if choices.len() > SCALAR_CHOICES_MAX_ITEMS {
        return Err(PyValueError::new_err(
            "choices snapshot exceeds ResourceLimitsV1",
        ));
    }
    Ok(Some(choices))
}

fn extract_string_choices(
    value: &Bound<'_, PyAny>,
    minimum: Option<usize>,
    maximum: usize,
) -> PyResult<Option<Vec<String>>> {
    let Some(choices) = extract_choice_tuple(value)? else {
        return Ok(None);
    };
    let mut output = Vec::with_capacity(choices.len());
    let mut seen = HashSet::with_capacity(choices.len());
    for choice in choices.iter() {
        let choice = extract_exact_string(&choice, "string choice snapshot must be an exact str")?;
        let scalar_count = choice.chars().count();
        if scalar_count > RESULT_STRING_MAX_SCALARS || choice.len() > RESULT_STRING_MAX_BYTES {
            return Err(PyValueError::new_err(
                "string choice snapshot exceeds ResourceLimitsV1",
            ));
        }
        if minimum.is_some_and(|minimum| scalar_count < minimum) || scalar_count > maximum {
            return Err(PyValueError::new_err(
                "string choice snapshot violates length bounds",
            ));
        }
        if !seen.insert(choice.clone()) {
            return Err(PyValueError::new_err(
                "choices snapshot must not contain duplicates",
            ));
        }
        output.push(choice);
    }
    Ok(Some(output))
}

fn extract_int64_choices(
    value: &Bound<'_, PyAny>,
    minimum: Option<i64>,
    maximum: Option<i64>,
) -> PyResult<Option<Vec<i64>>> {
    let Some(choices) = extract_choice_tuple(value)? else {
        return Ok(None);
    };
    let mut output = Vec::with_capacity(choices.len());
    let mut seen = HashSet::with_capacity(choices.len());
    for choice in choices.iter() {
        if !choice.get_type().is(choice.py().get_type::<PyInt>()) {
            return Err(PyTypeError::new_err(
                "integer choice snapshot must be an exact int",
            ));
        }
        let choice = choice.extract::<i64>()?;
        if minimum.is_some_and(|minimum| choice < minimum)
            || maximum.is_some_and(|maximum| choice > maximum)
        {
            return Err(PyValueError::new_err(
                "integer choice snapshot violates bounds",
            ));
        }
        if !seen.insert(choice) {
            return Err(PyValueError::new_err(
                "choices snapshot must not contain duplicates",
            ));
        }
        output.push(choice);
    }
    Ok(Some(output))
}

fn extract_float64_choices(
    value: &Bound<'_, PyAny>,
    minimum: Option<f64>,
    maximum: Option<f64>,
) -> PyResult<Option<Vec<f64>>> {
    let Some(choices) = extract_choice_tuple(value)? else {
        return Ok(None);
    };
    let mut output = Vec::with_capacity(choices.len());
    for choice in choices.iter() {
        if !choice.get_type().is(choice.py().get_type::<PyFloat>()) {
            return Err(PyTypeError::new_err(
                "float choice snapshot must be an exact float",
            ));
        }
        let choice = choice.extract::<f64>()?;
        if !choice.is_finite() {
            return Err(PyValueError::new_err(
                "float choice snapshot must be finite",
            ));
        }
        if minimum.is_some_and(|minimum| choice < minimum)
            || maximum.is_some_and(|maximum| choice > maximum)
        {
            return Err(PyValueError::new_err(
                "float choice snapshot violates bounds",
            ));
        }
        if output.contains(&choice) {
            return Err(PyValueError::new_err(
                "choices snapshot must not contain duplicates",
            ));
        }
        output.push(choice);
    }
    Ok(Some(output))
}

fn extract_bool_choices(value: &Bound<'_, PyAny>) -> PyResult<Option<Vec<bool>>> {
    let Some(choices) = extract_choice_tuple(value)? else {
        return Ok(None);
    };
    let mut output = Vec::with_capacity(choices.len());
    for choice in choices.iter() {
        let choice = extract_exact_bool(&choice, "boolean choice")?;
        if output.contains(&choice) {
            return Err(PyValueError::new_err(
                "choices snapshot must not contain duplicates",
            ));
        }
        output.push(choice);
    }
    Ok(Some(output))
}

pub(crate) fn schema_callback_error(
    py: Python<'_>,
    phase: &'static str,
    path: &str,
    cause: PyErr,
) -> PyErr {
    let wrapped = (|| -> PyResult<PyErr> {
        let class = py
            .import("troupe.act_schema")?
            .getattr("SchemaCallbackError")?;
        let kwargs = PyDict::new(py);
        kwargs.set_item("phase", phase)?;
        kwargs.set_item("path", path)?;
        let value = class.call(("schema callback failed",), Some(&kwargs))?;
        Ok(PyErr::from_value(value))
    })();
    match wrapped {
        Ok(error) => {
            error.set_cause(py, Some(cause));
            error
        }
        Err(error) => {
            error.set_cause(py, Some(cause));
            error
        }
    }
}

fn render_object(contract: &ObjectContract, indent: usize, output: &mut String) {
    output.push_str("{\n");
    for (index, field) in contract.fields.iter().enumerate() {
        output.push_str(&" ".repeat(indent + 2));
        output
            .push_str(&serde_json::to_string(&field.name).expect("a field name always serializes"));
        if !field.required {
            output.push('?');
        }
        output.push_str(": ");
        render_value(&field.value, indent + 2, output);
        if index + 1 != contract.fields.len() {
            output.push(',');
        }
        output.push('\n');
    }
    output.push_str(&" ".repeat(indent));
    output.push('}');
}

fn render_value(contract: &ValueContract, indent: usize, output: &mut String) {
    match contract {
        ValueContract::String {
            description,
            min_length,
            max_length,
            choices,
        } => {
            output.push_str("<string; ");
            render_prompt_text("description", description, output);
            if let Some(minimum) = min_length {
                output.push_str(&format!("; minimum length {minimum}"));
            }
            output.push_str(&format!("; maximum length {max_length}"));
            if let Some(choices) = choices {
                output.push_str("; one of ");
                output.push_str(&serde_json::to_string(choices).unwrap());
            }
            output.push('>');
        }
        ValueContract::Int64 {
            description,
            min,
            max,
            choices,
        } => {
            output.push_str("<int64; ");
            render_prompt_text("description", description, output);
            if min.is_some() || max.is_some() {
                output.push_str(&format!(
                    "; inclusive range {}..{}",
                    min.map_or_else(|| "int64-min".to_owned(), |value| value.to_string()),
                    max.map_or_else(|| "int64-max".to_owned(), |value| value.to_string())
                ));
            }
            if let Some(choices) = choices {
                output.push_str("; one of ");
                output.push_str(&serde_json::to_string(choices).unwrap());
            }
            output.push('>');
        }
        ValueContract::Float64 {
            description,
            min,
            max,
            choices,
        } => {
            output.push_str("<float64; ");
            render_prompt_text("description", description, output);
            if min.is_some() || max.is_some() {
                output.push_str(&format!(
                    "; inclusive range {}..{}",
                    min.map_or_else(|| "float64-min".to_owned(), |value| value.to_string()),
                    max.map_or_else(|| "float64-max".to_owned(), |value| value.to_string())
                ));
            }
            if let Some(choices) = choices {
                output.push_str("; one of ");
                output.push_str(&serde_json::to_string(choices).unwrap());
            }
            output.push('>');
        }
        ValueContract::Bool {
            description,
            choices,
        } => {
            output.push_str("<bool; ");
            render_prompt_text("description", description, output);
            if let Some(choices) = choices {
                output.push_str("; one of ");
                output.push_str(&serde_json::to_string(choices).unwrap());
            }
            output.push('>');
        }
        ValueContract::List {
            description,
            item,
            min_items,
            max_items,
        } => {
            output.push_str("<array; ");
            render_prompt_text("description", description, output);
            if let Some(minimum) = min_items {
                output.push_str(&format!("; minimum items {minimum}"));
            }
            output.push_str(&format!("; maximum items {max_items}; items "));
            render_value(item, indent, output);
            output.push('>');
        }
        ValueContract::Object {
            description,
            contract,
        } => {
            output.push_str("<object; ");
            render_prompt_text("description", description, output);
            output.push_str("> ");
            render_object(contract, indent, output);
        }
        ValueContract::Nullable(inner) => {
            render_value(inner, indent, output);
            output.push_str(" or null");
        }
        ValueContract::Custom {
            description,
            json_kind,
            prompt_fragment,
            ..
        } => {
            output.push('<');
            output.push_str(json_kind.name());
            output.push_str("; ");
            render_prompt_text("description", description, output);
            output.push_str("; ");
            render_prompt_text("custom_constraint", prompt_fragment, output);
            output.push('>');
        }
    }
}

fn render_prompt_text(label: &str, value: &str, output: &mut String) {
    output.push_str(label);
    output.push('=');
    output.push_str(
        &serde_json::to_string(value).expect("a Rust string always has a JSON representation"),
    );
}

#[derive(Default)]
struct IssueCollector {
    items: Vec<ValidationIssue>,
    truncated: bool,
}

impl IssueCollector {
    fn push(&mut self, path: &str, code: &'static str, message: impl Into<String>) {
        if self.items.len() < MAX_VALIDATION_ISSUES {
            self.items.push(ValidationIssue {
                path: path.to_owned(),
                code,
                message: message.into(),
            });
        } else {
            self.truncated = true;
        }
    }
}

fn check_result_resources(
    value: &JsonValue,
    path: &str,
    depth: usize,
    nodes: &mut usize,
) -> Option<ValidationIssue> {
    *nodes += 1;
    if depth > RESULT_MAX_DEPTH {
        return Some(resource_issue(
            path,
            format!(
                "expected result depth at most {RESULT_MAX_DEPTH}, got {} beyond the depth limit",
                actual_kind(value)
            ),
        ));
    }
    if *nodes > RESULT_MAX_NODES {
        return Some(resource_issue(
            path,
            format!(
                "expected at most {RESULT_MAX_NODES} result nodes, got {} beyond the node limit",
                actual_kind(value)
            ),
        ));
    }
    match value {
        JsonValue::String(value) => {
            if let Some(issue) = string_resource_issue(value, path, "string") {
                return Some(issue);
            }
        }
        JsonValue::Array(values) => {
            if values.len() > RESULT_LIST_MAX_ITEMS {
                return Some(resource_issue(
                    path,
                    format!(
                        "expected array with at most {RESULT_LIST_MAX_ITEMS} items, got array with \
                         {} items",
                        values.len()
                    ),
                ));
            }
            for (index, value) in values.iter().enumerate() {
                if let Some(issue) =
                    check_result_resources(value, &format!("{path}/{index}"), depth + 1, nodes)
                {
                    return Some(issue);
                }
            }
        }
        JsonValue::Object(values) => {
            for (name, value) in values {
                if let Some(issue) = string_resource_issue(name, path, "object key") {
                    return Some(issue);
                }
                if let Some(issue) =
                    check_result_resources(value, &join_path(path, name), depth + 1, nodes)
                {
                    return Some(issue);
                }
            }
        }
        _ => {}
    }
    None
}

fn string_resource_issue(value: &str, path: &str, subject: &str) -> Option<ValidationIssue> {
    let scalars = value.chars().count();
    if scalars <= RESULT_STRING_MAX_SCALARS && value.len() <= RESULT_STRING_MAX_BYTES {
        return None;
    }
    Some(resource_issue(
        path,
        format!(
            "expected {subject} with at most {RESULT_STRING_MAX_SCALARS} scalars and \
             {RESULT_STRING_MAX_BYTES} UTF-8 bytes, got {subject} with {scalars} scalars and {} \
             UTF-8 bytes",
            value.len()
        ),
    ))
}

fn resource_issue(path: &str, message: String) -> ValidationIssue {
    ValidationIssue {
        path: path.to_owned(),
        code: "resource_limit",
        message,
    }
}

fn actual_kind(value: &JsonValue) -> &'static str {
    match value {
        JsonValue::Null => "null",
        JsonValue::Bool(_) => "boolean",
        JsonValue::Number(number) if number.is_i64() => "int64",
        JsonValue::Number(number) if number.is_u64() => "integer outside int64",
        JsonValue::Number(number)
            if !number
                .to_string()
                .bytes()
                .any(|byte| matches!(byte, b'.' | b'e' | b'E')) =>
        {
            "integer outside int64"
        }
        JsonValue::Number(_) => "number",
        JsonValue::String(_) => "string",
        JsonValue::Array(_) => "array",
        JsonValue::Object(_) => "object",
    }
}

fn string_constraint(
    min_length: Option<usize>,
    max_length: usize,
    choices: &Option<Vec<String>>,
) -> String {
    let mut constraint = match min_length {
        Some(minimum) => {
            format!("string with scalar length in inclusive range [{minimum}, {max_length}]")
        }
        None => format!("string with at most {max_length} scalars"),
    };
    if let Some(choices) = choices {
        constraint.push_str(" and value one of ");
        constraint.push_str(&serde_json::to_string(choices).expect("string choices serialize"));
    }
    constraint
}

fn int64_constraint(min: Option<i64>, max: Option<i64>, choices: &Option<Vec<i64>>) -> String {
    let mut constraint = match (min, max) {
        (Some(minimum), Some(maximum)) => {
            format!("int64 in inclusive range [{minimum}, {maximum}]")
        }
        (Some(minimum), None) => format!("int64 greater than or equal to {minimum}"),
        (None, Some(maximum)) => format!("int64 less than or equal to {maximum}"),
        (None, None) => "int64".to_owned(),
    };
    if let Some(choices) = choices {
        constraint.push_str(" and one of ");
        constraint.push_str(&serde_json::to_string(choices).expect("int64 choices serialize"));
    }
    constraint
}

fn float64_constraint(min: Option<f64>, max: Option<f64>, choices: &Option<Vec<f64>>) -> String {
    let mut constraint = match (min, max) {
        (Some(minimum), Some(maximum)) => {
            format!("finite float64 in inclusive range [{minimum}, {maximum}]")
        }
        (Some(minimum), None) => format!("finite float64 greater than or equal to {minimum}"),
        (None, Some(maximum)) => format!("finite float64 less than or equal to {maximum}"),
        (None, None) => "finite float64".to_owned(),
    };
    if let Some(choices) = choices {
        constraint.push_str(" and one of ");
        constraint
            .push_str(&serde_json::to_string(choices).expect("finite float64 choices serialize"));
    }
    constraint
}

fn bool_constraint(choices: &Option<Vec<bool>>) -> String {
    let mut constraint = "boolean".to_owned();
    if let Some(choices) = choices {
        constraint.push_str(" one of ");
        constraint.push_str(&serde_json::to_string(choices).expect("boolean choices serialize"));
    }
    constraint
}

fn array_constraint(min_items: Option<usize>, max_items: usize) -> String {
    match min_items {
        Some(minimum) => {
            format!("array with item count in inclusive range [{minimum}, {max_items}]")
        }
        None => format!("array with at most {max_items} items"),
    }
}

fn validate_object(
    contract: &ObjectContract,
    value: &JsonValue,
    path: &str,
    issues: &mut IssueCollector,
    custom_jobs: &mut Vec<CustomValidationJob>,
) -> Option<ValidatedActValue> {
    let JsonValue::Object(values) = value else {
        issues.push(
            path,
            "type_mismatch",
            format!("expected object, got {}", actual_kind(value)),
        );
        return None;
    };
    let mut output = Vec::new();
    let expected = contract
        .fields
        .iter()
        .map(|field| field.name.as_str())
        .collect::<HashSet<_>>();
    for field in &contract.fields {
        let field_path = join_path(path, &field.name);
        match values.get(&field.name) {
            Some(value) => {
                if let Some(value) =
                    validate_value(&field.value, value, &field_path, issues, custom_jobs)
                {
                    output.push((field.name.clone(), value));
                }
            }
            None if field.required => {
                issues.push(
                    &field_path,
                    "missing_field",
                    "expected required field, got missing field",
                );
            }
            None => {}
        }
    }
    let mut extras = values
        .keys()
        .filter(|name| !expected.contains(name.as_str()))
        .collect::<Vec<_>>();
    extras.sort();
    for name in extras {
        issues.push(
            &join_path(path, name),
            "extra_field",
            "expected a declared field, got extra field",
        );
    }
    Some(ValidatedActValue::Object(output))
}

fn validate_value(
    contract: &ValueContract,
    value: &JsonValue,
    path: &str,
    issues: &mut IssueCollector,
    custom_jobs: &mut Vec<CustomValidationJob>,
) -> Option<ValidatedActValue> {
    match contract {
        ValueContract::String {
            min_length,
            max_length,
            choices,
            ..
        } => {
            let JsonValue::String(value) = value else {
                issues.push(
                    path,
                    "type_mismatch",
                    format!(
                        "expected {}, got {}",
                        string_constraint(*min_length, *max_length, choices),
                        actual_kind(value)
                    ),
                );
                return None;
            };
            let length = value.chars().count();
            if min_length.is_some_and(|minimum| length < minimum) || length > *max_length {
                issues.push(
                    path,
                    "length_limit",
                    format!(
                        "expected {}, got string with {length} scalars",
                        string_constraint(*min_length, *max_length, choices)
                    ),
                );
                return None;
            }
            if choices
                .as_ref()
                .is_some_and(|choices| !choices.contains(value))
            {
                issues.push(
                    path,
                    "not_in_choices",
                    format!(
                        "expected string one of {}, got string outside the allowed choices",
                        serde_json::to_string(choices.as_ref().expect("checked choices"))
                            .expect("string choices always serialize")
                    ),
                );
                return None;
            }
            Some(ValidatedActValue::String(value.clone()))
        }
        ValueContract::Int64 {
            min, max, choices, ..
        } => {
            let Some(value) = value.as_i64() else {
                issues.push(
                    path,
                    "type_mismatch",
                    format!(
                        "expected {}, got {}",
                        int64_constraint(*min, *max, choices),
                        actual_kind(value)
                    ),
                );
                return None;
            };
            if min.is_some_and(|minimum| value < minimum)
                || max.is_some_and(|maximum| value > maximum)
            {
                issues.push(
                    path,
                    "out_of_range",
                    format!(
                        "expected {}, got int64 outside the accepted range",
                        int64_constraint(*min, *max, choices)
                    ),
                );
                return None;
            }
            if choices
                .as_ref()
                .is_some_and(|choices| !choices.contains(&value))
            {
                issues.push(
                    path,
                    "not_in_choices",
                    format!(
                        "expected int64 one of {}, got int64 outside the allowed choices",
                        serde_json::to_string(choices.as_ref().expect("checked choices"))
                            .expect("int64 choices always serialize")
                    ),
                );
                return None;
            }
            Some(ValidatedActValue::Int64(value))
        }
        ValueContract::Float64 {
            min, max, choices, ..
        } => {
            let Some(value) = value.as_f64().filter(|value| value.is_finite()) else {
                issues.push(
                    path,
                    "type_mismatch",
                    format!(
                        "expected {}, got {}",
                        float64_constraint(*min, *max, choices),
                        actual_kind(value)
                    ),
                );
                return None;
            };
            if min.is_some_and(|minimum| value < minimum)
                || max.is_some_and(|maximum| value > maximum)
            {
                issues.push(
                    path,
                    "out_of_range",
                    format!(
                        "expected {}, got number outside the accepted range",
                        float64_constraint(*min, *max, choices)
                    ),
                );
                return None;
            }
            if choices
                .as_ref()
                .is_some_and(|choices| !choices.contains(&value))
            {
                issues.push(
                    path,
                    "not_in_choices",
                    format!(
                        "expected float64 one of {}, got number outside the allowed choices",
                        serde_json::to_string(choices.as_ref().expect("checked choices"))
                            .expect("finite float64 choices always serialize")
                    ),
                );
                return None;
            }
            Some(ValidatedActValue::Float64(value))
        }
        ValueContract::Bool { choices, .. } => {
            let Some(value) = value.as_bool() else {
                issues.push(
                    path,
                    "type_mismatch",
                    format!(
                        "expected {}, got {}",
                        bool_constraint(choices),
                        actual_kind(value)
                    ),
                );
                return None;
            };
            if choices
                .as_ref()
                .is_some_and(|choices| !choices.contains(&value))
            {
                issues.push(
                    path,
                    "not_in_choices",
                    format!(
                        "expected boolean one of {}, got boolean outside the allowed choices",
                        serde_json::to_string(choices.as_ref().expect("checked choices"))
                            .expect("boolean choices always serialize")
                    ),
                );
                return None;
            }
            Some(ValidatedActValue::Bool(value))
        }
        ValueContract::List {
            item,
            min_items,
            max_items,
            ..
        } => {
            let JsonValue::Array(values) = value else {
                issues.push(
                    path,
                    "type_mismatch",
                    format!(
                        "expected {}, got {}",
                        array_constraint(*min_items, *max_items),
                        actual_kind(value)
                    ),
                );
                return None;
            };
            if min_items.is_some_and(|minimum| values.len() < minimum) || values.len() > *max_items
            {
                issues.push(
                    path,
                    "item_limit",
                    format!(
                        "expected {}, got array with {} items",
                        array_constraint(*min_items, *max_items),
                        values.len()
                    ),
                );
            }
            let output = values
                .iter()
                .enumerate()
                .filter_map(|(index, value)| {
                    validate_value(item, value, &format!("{path}/{index}"), issues, custom_jobs)
                })
                .collect();
            Some(ValidatedActValue::List(output))
        }
        ValueContract::Object { contract, .. } => {
            validate_object(contract, value, path, issues, custom_jobs)
        }
        ValueContract::Nullable(inner) if value.is_null() => Some(ValidatedActValue::Null),
        ValueContract::Nullable(inner) => validate_value(inner, value, path, issues, custom_jobs),
        ValueContract::Custom {
            json_kind,
            validator_id,
            ..
        } => match validate_custom_kind(*json_kind, value) {
            Some(validated) => {
                custom_jobs.push(CustomValidationJob {
                    validator_id: *validator_id,
                    path: path.to_owned(),
                    value: validated.clone(),
                });
                Some(validated)
            }
            None => {
                issues.push(
                    path,
                    "type_mismatch",
                    format!(
                        "expected custom {} value, got {}",
                        json_kind.name(),
                        actual_kind(value)
                    ),
                );
                None
            }
        },
    }
}

fn validate_custom_kind(kind: JsonKind, value: &JsonValue) -> Option<ValidatedActValue> {
    match kind {
        JsonKind::String => value
            .as_str()
            .map(|value| ValidatedActValue::String(value.to_owned())),
        JsonKind::Int64 => value.as_i64().map(ValidatedActValue::Int64),
        JsonKind::Float64 => value
            .as_f64()
            .filter(|value| value.is_finite())
            .map(ValidatedActValue::Float64),
        JsonKind::Bool => value.as_bool().map(ValidatedActValue::Bool),
        JsonKind::Array if value.is_array() => validated_json_value(value),
        JsonKind::Object if value.is_object() => validated_json_value(value),
        JsonKind::Array | JsonKind::Object => None,
    }
}

fn validated_json_value(value: &JsonValue) -> Option<ValidatedActValue> {
    match value {
        JsonValue::Null => Some(ValidatedActValue::Null),
        JsonValue::Bool(value) => Some(ValidatedActValue::Bool(*value)),
        JsonValue::Number(value) => value
            .as_i64()
            .map(ValidatedActValue::Int64)
            .or_else(|| value.as_u64().map(ValidatedActValue::UInt64))
            .or_else(|| {
                let encoded = value.to_string();
                (!encoded
                    .bytes()
                    .any(|byte| matches!(byte, b'.' | b'e' | b'E')))
                .then_some(ValidatedActValue::BigInt(encoded))
            })
            .or_else(|| value.as_f64().map(ValidatedActValue::Float64)),
        JsonValue::String(value) => Some(ValidatedActValue::String(value.clone())),
        JsonValue::Array(values) => values
            .iter()
            .map(validated_json_value)
            .collect::<Option<Vec<_>>>()
            .map(ValidatedActValue::List),
        JsonValue::Object(values) => values
            .iter()
            .map(|(name, value)| Some((name.clone(), validated_json_value(value)?)))
            .collect::<Option<Vec<_>>>()
            .map(ValidatedActValue::Object),
    }
}

fn join_path(parent: &str, name: &str) -> String {
    let escaped = name.replace('~', "~0").replace('/', "~1");
    format!("{parent}/{escaped}")
}

pub(crate) fn install(module: &Bound<'_, PyModule>) -> PyResult<()> {
    let py = module.py();
    let source = CString::new(ACT_SCHEMA_API).expect("embedded act_schema source contains no NUL");
    let act_schema = PyModule::from_code(
        py,
        source.as_c_str(),
        c"<troupe.act_schema>",
        c"troupe.act_schema",
    )?;
    module.add_submodule(&act_schema)
}

#[cfg(test)]
mod tests {
    use std::ffi::CString;

    use pyo3::exceptions::PyTypeError;
    use pyo3::prelude::*;
    use pyo3::types::{PyDict, PyList, PyModule, PyTuple};
    use serde_json::json;

    use super::{
        NativeValidationOutcome, SchemaValidationMode, ValidatedActValue, compile_act_schema,
    };

    fn module(py: Python<'_>) -> Bound<'_, PyModule> {
        let source = CString::new(super::ACT_SCHEMA_API).unwrap();
        let act_schema = PyModule::from_code(
            py,
            source.as_c_str(),
            c"<troupe.act_schema-test>",
            c"troupe.act_schema",
        )
        .unwrap();
        let troupe = PyModule::new(py, "troupe").unwrap();
        troupe.setattr("__path__", PyList::empty(py)).unwrap();
        troupe.add("act_schema", &act_schema).unwrap();
        py.import("sys")
            .unwrap()
            .getattr("modules")
            .unwrap()
            .cast_into::<PyDict>()
            .unwrap()
            .set_item("troupe", troupe)
            .unwrap();
        act_schema
    }

    #[test]
    fn arbitrary_precision_integer_materialization_bypasses_decimal_digit_limit() {
        let _guard = crate::initialize_python_for_test();
        Python::attach(|py| {
            let positive = ValidatedActValue::BigInt("9".repeat(5_000))
                .into_py(py)
                .unwrap();
            let negative = ValidatedActValue::BigInt(format!("-{}", "9".repeat(5_001)))
                .into_py(py)
                .unwrap();
            assert!(
                positive
                    .bind(py)
                    .eq(py.eval(c"10**5000 - 1", None, None).unwrap())
                    .unwrap()
            );
            assert!(
                negative
                    .bind(py)
                    .eq(py.eval(c"-(10**5001 - 1)", None, None).unwrap())
                    .unwrap()
            );
        });
    }

    #[test]
    fn compiler_prompt_and_native_validator_share_one_ordered_ir() {
        let _guard = crate::initialize_python_for_test();
        Python::attach(|py| {
            let locals = PyDict::new(py);
            locals.set_item("act_schema", module(py)).unwrap();
            py.run(
                c"schema = {\n\
                  'decision': act_schema.StrValue(description='decision', choices=['approve', 'reject']),\n\
                  'score': act_schema.Int64Value(description='score', min=0, max=100),\n\
                  'ratio': act_schema.Float64Value(description='ratio', min=0.0, max=1.0),\n\
                  'metadata': act_schema.ObjectValue(description='metadata', fields={\n\
                    'note': act_schema.Field(act_schema.NullableValue(act_schema.StrValue(description='note')), required=False),\n\
                    'flags': act_schema.ListValue(act_schema.BoolValue(description='flag'), description='flags', max_items=2),\n\
                  }),\n\
                }",
                None,
                Some(&locals),
            )
            .unwrap();

            let compiled =
                compile_act_schema(&locals.get_item("schema").unwrap().unwrap()).unwrap();
            assert_eq!(compiled.validation_mode(), SchemaValidationMode::NativeOnly);
            let prompt = compiled.render_prompt("Inspect the repository.").unwrap();
            assert!(prompt.contains("Inspect the repository."));
            let positions =
                ["decision", "score", "ratio", "metadata"].map(|field| prompt.find(field).unwrap());
            assert!(positions.windows(2).all(|pair| pair[0] < pair[1]));
            assert!(prompt.contains("approve"));
            assert!(prompt.contains("inclusive range 0..100"));
            assert!(prompt.contains("inclusive range 0..1"));
            assert!(!prompt.contains("Some("));
            assert!(prompt.contains("note"));

            let valid = json!({
                "decision": "approve",
                "score": 100,
                "ratio": 1,
                "metadata": {"note": null, "flags": [true, false]},
            });
            assert_eq!(
                compiled.validate(&valid),
                NativeValidationOutcome::Valid {
                    value: ValidatedActValue::Object(vec![
                        (
                            "decision".to_owned(),
                            ValidatedActValue::String("approve".to_owned())
                        ),
                        ("score".to_owned(), ValidatedActValue::Int64(100)),
                        ("ratio".to_owned(), ValidatedActValue::Float64(1.0)),
                        (
                            "metadata".to_owned(),
                            ValidatedActValue::Object(vec![
                                ("note".to_owned(), ValidatedActValue::Null),
                                (
                                    "flags".to_owned(),
                                    ValidatedActValue::List(vec![
                                        ValidatedActValue::Bool(true),
                                        ValidatedActValue::Bool(false),
                                    ]),
                                ),
                            ]),
                        ),
                    ]),
                    custom_jobs: Vec::new(),
                },
            );

            let invalid = json!({
                "decision": "maybe",
                "ratio": true,
                "metadata": {"flags": [true, false, true], "extra": 1},
                "unexpected": false,
            });
            let NativeValidationOutcome::Invalid { issues, truncated } =
                compiled.validate(&invalid)
            else {
                panic!("invalid value was accepted");
            };
            assert!(!truncated);
            assert_eq!(
                issues
                    .iter()
                    .map(|issue| (issue.path.as_str(), issue.code))
                    .collect::<Vec<_>>(),
                vec![
                    ("/decision", "not_in_choices"),
                    ("/score", "missing_field"),
                    ("/ratio", "type_mismatch"),
                    ("/metadata/flags", "item_limit"),
                    ("/metadata/extra", "extra_field"),
                    ("/unexpected", "extra_field"),
                ],
            );
        });
    }

    #[test]
    fn custom_nodes_render_once_and_produce_ordered_jobs_after_native_success() {
        let _guard = crate::initialize_python_for_test();
        Python::attach(|py| {
            let locals = PyDict::new(py);
            locals.set_item("act_schema", module(py)).unwrap();
            let source = CString::new(
                r#"class CustomInt(act_schema.SchemaValue):
    def __init__(self):
        super().__init__(description='custom integer', json_kind='int64')
        self.render_calls = 0
        self.fragment = 'must be divisible by seven'
    def render_prompt(self):
        self.render_calls += 1
        return self.fragment
    def validate(self, value):
        return None

custom = CustomInt()
schema = {
    'first': custom,
    'items': act_schema.ListValue(custom, description='items'),
}
"#,
            )
            .unwrap();
            py.run(source.as_c_str(), None, Some(&locals)).unwrap();
            let custom = locals.get_item("custom").unwrap().unwrap();
            let compiled =
                compile_act_schema(&locals.get_item("schema").unwrap().unwrap()).unwrap();

            assert_eq!(compiled.validation_mode(), SchemaValidationMode::Hybrid);
            assert_eq!(
                custom
                    .getattr("render_calls")
                    .unwrap()
                    .extract::<usize>()
                    .unwrap(),
                1,
            );
            custom
                .setattr("fragment", "replacement after preflight")
                .unwrap();
            let prompt = compiled.render_prompt("script").unwrap();
            assert_eq!(prompt.matches("must be divisible by seven").count(), 2);
            assert!(!prompt.contains("replacement after preflight"));

            let NativeValidationOutcome::Valid { custom_jobs, .. } =
                compiled.validate(&json!({"first": 7, "items": [14, 21]}))
            else {
                panic!("custom value with valid native kinds was rejected");
            };
            assert_eq!(
                custom_jobs
                    .iter()
                    .map(|job| (job.validator_id, job.path.as_str(), &job.value))
                    .collect::<Vec<_>>(),
                vec![
                    (0, "/first", &ValidatedActValue::Int64(7)),
                    (0, "/items/0", &ValidatedActValue::Int64(14)),
                    (0, "/items/1", &ValidatedActValue::Int64(21)),
                ],
            );

            assert!(matches!(
                compiled.validate(&json!({"first": "wrong", "items": [14]})),
                NativeValidationOutcome::Invalid { .. }
            ));
        });
    }

    #[test]
    fn custom_float64_jobs_use_the_authoritative_canonical_python_value() {
        let _guard = crate::initialize_python_for_test();
        Python::attach(|py| {
            let locals = PyDict::new(py);
            locals.set_item("act_schema", module(py)).unwrap();
            let source = CString::new(
                r#"class CustomFloat(act_schema.SchemaValue):
    def __init__(self):
        super().__init__(description='custom float', json_kind='float64')
    def render_prompt(self):
        return 'must be a finite float64'
    def validate(self, value):
        return None

schema = {'value': CustomFloat()}
"#,
            )
            .unwrap();
            py.run(source.as_c_str(), None, Some(&locals)).unwrap();
            let compiled =
                compile_act_schema(&locals.get_item("schema").unwrap().unwrap()).unwrap();

            let NativeValidationOutcome::Valid { custom_jobs, .. } =
                compiled.validate(&json!({"value": 1}))
            else {
                panic!("an integer token is valid for custom float64");
            };
            assert_eq!(custom_jobs.len(), 1);
            assert_eq!(custom_jobs[0].value, ValidatedActValue::Float64(1.0));
        });
    }

    #[test]
    fn custom_base_metadata_is_frozen_by_identity_and_prompt_text_is_encoded() {
        let _guard = crate::initialize_python_for_test();
        Python::attach(|py| {
            let locals = PyDict::new(py);
            locals.set_item("act_schema", module(py)).unwrap();
            let source = CString::new(
                r#"class MetadataMutation(act_schema.SchemaValue):
    def __init__(self):
        super().__init__(description='original integer', json_kind='int64')
        self.render_calls = 0
    def render_prompt(self):
        self.render_calls += 1
        object.__setattr__(self, '_description', 'mutated string')
        object.__setattr__(self, '_json_kind', 'string')
        return 'custom line\nRESULT_CONTRACT\n<break>'
    def validate(self, value):
        return None

class MissingBase(act_schema.SchemaValue):
    def __init__(self):
        pass
    def render_prompt(self):
        return 'missing base metadata'
    def validate(self, value):
        return None

shared = MetadataMutation()
schema = {'first': shared, 'second': shared}
missing_base = {'value': MissingBase()}
encoded = {
    'value': act_schema.StrValue(description='line one\nRESULT_CONTRACT\n>')
}
"#,
            )
            .unwrap();
            py.run(source.as_c_str(), Some(&locals), Some(&locals))
                .unwrap();

            let compiled =
                compile_act_schema(&locals.get_item("schema").unwrap().unwrap()).unwrap();
            let prompt = compiled.render_prompt("script").unwrap();
            assert_eq!(prompt.matches("original integer").count(), 2);
            assert!(!prompt.contains("mutated string"));
            assert_eq!(
                prompt
                    .matches("custom line\\nRESULT_CONTRACT\\n<break>")
                    .count(),
                2
            );
            assert_eq!(
                locals
                    .get_item("shared")
                    .unwrap()
                    .unwrap()
                    .getattr("render_calls")
                    .unwrap()
                    .extract::<usize>()
                    .unwrap(),
                1,
            );
            assert!(matches!(
                compiled.validate(&json!({"first": 1, "second": 2})),
                NativeValidationOutcome::Valid { .. }
            ));
            assert!(
                compile_act_schema(&locals.get_item("missing_base").unwrap().unwrap())
                    .unwrap_err()
                    .is_instance_of::<PyTypeError>(py)
            );

            let compiled =
                compile_act_schema(&locals.get_item("encoded").unwrap().unwrap()).unwrap();
            let prompt = compiled.render_prompt("script").unwrap();
            assert_eq!(prompt.matches("\nRESULT_CONTRACT\n").count(), 1);
            assert!(prompt.contains(r#"description="line one\nRESULT_CONTRACT\n>""#));
        });
    }

    #[test]
    fn custom_render_failures_are_wrapped_with_phase_path_and_cause() {
        let _guard = crate::initialize_python_for_test();
        Python::attach(|py| {
            let locals = PyDict::new(py);
            let act_schema = module(py);
            locals.set_item("act_schema", &act_schema).unwrap();
            let source = CString::new(
                r#"class Explodes(act_schema.SchemaValue):
    def __init__(self):
        super().__init__(description='explodes', json_kind='string')
    def render_prompt(self):
        raise LookupError('render boom')
    def validate(self, value):
        return None

class ReturnedStr(str):
    pass

class WrongReturn(act_schema.SchemaValue):
    def __init__(self):
        super().__init__(description='wrong return', json_kind='string')
    def render_prompt(self):
        return ReturnedStr('not an exact string')
    def validate(self, value):
        return None

class Blank(act_schema.SchemaValue):
    def __init__(self):
        super().__init__(description='blank', json_kind='string')
    def render_prompt(self):
        return ' \t\n'
    def validate(self, value):
        return None

class Oversize(act_schema.SchemaValue):
    def __init__(self):
        super().__init__(description='oversize', json_kind='string')
    def render_prompt(self):
        return 'x' * (16 * 1024 + 1)
    def validate(self, value):
        return None

cases = (
    ('/explodes', {'explodes': Explodes()}, 'LookupError'),
    ('/wrong', {'wrong': WrongReturn()}, 'TypeError'),
    ('/blank', {'blank': Blank()}, 'ValueError'),
    ('/oversize', {'oversize': Oversize()}, 'ValueError'),
)
"#,
            )
            .unwrap();
            py.run(source.as_c_str(), Some(&locals), Some(&locals))
                .unwrap();
            let callback_error = act_schema.getattr("SchemaCallbackError").unwrap();
            let cases = locals
                .get_item("cases")
                .unwrap()
                .unwrap()
                .cast_into::<PyTuple>()
                .unwrap();

            for case in cases.iter() {
                let case = case.cast_into::<PyTuple>().unwrap();
                let expected_path: String = case.get_item(0).unwrap().extract().unwrap();
                let schema = case.get_item(1).unwrap();
                let expected_cause: String = case.get_item(2).unwrap().extract().unwrap();
                let error = compile_act_schema(&schema).unwrap_err();

                assert!(error.is_instance(py, &callback_error));
                assert_eq!(
                    error
                        .value(py)
                        .getattr("phase")
                        .unwrap()
                        .extract::<String>()
                        .unwrap(),
                    "render_prompt",
                );
                assert_eq!(
                    error
                        .value(py)
                        .getattr("path")
                        .unwrap()
                        .extract::<String>()
                        .unwrap(),
                    expected_path,
                );
                let cause = error.cause(py).expect("render failures preserve a cause");
                assert_eq!(
                    cause
                        .value(py)
                        .get_type()
                        .getattr("__name__")
                        .unwrap()
                        .extract::<String>()
                        .unwrap(),
                    expected_cause,
                );
                if expected_path == "/explodes" {
                    assert_eq!(
                        cause.value(py).str().unwrap().to_str().unwrap(),
                        "render boom",
                    );
                }
            }
        });
    }

    #[test]
    fn result_array_resource_limit_applies_before_custom_validation() {
        let _guard = crate::initialize_python_for_test();
        Python::attach(|py| {
            let locals = PyDict::new(py);
            locals.set_item("act_schema", module(py)).unwrap();
            let source = CString::new(
                r#"class CustomArray(act_schema.SchemaValue):
    def __init__(self):
        super().__init__(description='custom array', json_kind='array')
    def render_prompt(self):
        return 'array accepted by custom validation'
    def validate(self, value):
        return None

schema = {'items': CustomArray()}
"#,
            )
            .unwrap();
            py.run(source.as_c_str(), None, Some(&locals)).unwrap();
            let compiled =
                compile_act_schema(&locals.get_item("schema").unwrap().unwrap()).unwrap();

            for item_count in [9_999, 10_000] {
                let value = json!({"items": vec![0; item_count]});
                let NativeValidationOutcome::Valid { custom_jobs, .. } = compiled.validate(&value)
                else {
                    panic!("array at or below ResourceLimitsV1 was rejected");
                };
                assert_eq!(custom_jobs.len(), 1);
            }

            let NativeValidationOutcome::Invalid { issues, truncated } =
                compiled.validate(&json!({"items": vec![0; 10_001]}))
            else {
                panic!("array above ResourceLimitsV1 was accepted");
            };
            assert!(!truncated);
            assert_eq!(
                issues
                    .iter()
                    .map(|issue| (issue.path.as_str(), issue.code))
                    .collect::<Vec<_>>(),
                vec![("/items", "resource_limit")],
            );
        });
    }

    #[test]
    fn resource_failure_short_circuits_schema_materialization_and_issue_walking() {
        let _guard = crate::initialize_python_for_test();
        Python::attach(|py| {
            let locals = PyDict::new(py);
            locals.set_item("act_schema", module(py)).unwrap();
            py.run(
                c"schema = {'items': act_schema.ListValue(\
                  act_schema.Int64Value(description='item'), description='items')}",
                None,
                Some(&locals),
            )
            .unwrap();
            let compiled =
                compile_act_schema(&locals.get_item("schema").unwrap().unwrap()).unwrap();

            let NativeValidationOutcome::Invalid { issues, truncated } =
                compiled.validate(&json!({"items": vec!["wrong"; 10_001]}))
            else {
                panic!("an oversized array was accepted");
            };
            assert!(!truncated);
            assert_eq!(issues.len(), 1);
            assert_eq!(issues[0].code, "resource_limit");
            assert!(issues[0].message.contains("expected"));
            assert!(issues[0].message.contains("got array"));
        });
    }

    #[test]
    fn compiled_schema_and_prompt_enforce_every_exact_aggregate_boundary() {
        let _guard = crate::initialize_python_for_test();
        Python::attach(|py| {
            let locals = PyDict::new(py);
            locals.set_item("act_schema", module(py)).unwrap();
            let source = CString::new(
                r#"def chain(length):
    value = act_schema.StrValue(description='value')
    for _ in range(length - 1):
        value = act_schema.NullableValue(value)
    return value

def schema_for_depth(length):
    return {'value': chain(length)}

def schema_for_nodes(lengths):
    return {str(index): chain(length) for index, length in enumerate(lengths)}

def schema_for_fields(count):
    return {
        f'field_{index}': act_schema.StrValue(description='value')
        for index in range(count)
    }

def schema_for_choice_counts(counts):
    return {
        f'field_{field}': act_schema.StrValue(
            description='choice',
            choices=tuple(f'{field}-{choice}' for choice in range(count)),
        )
        for field, count in enumerate(counts)
    }

def schema_for_choice_bytes(length):
    return {
        'value': act_schema.StrValue(
            description='choice bytes',
            choices=('x' * length,),
        )
    }

def schema_for_prompt(name_length):
    return {
        'x' * name_length: act_schema.StrValue(description='prompt value')
    }
"#,
            )
            .unwrap();
            py.run(source.as_c_str(), Some(&locals), Some(&locals))
                .unwrap();

            let depth = locals.get_item("schema_for_depth").unwrap().unwrap();
            for length in [30, 31] {
                let schema = depth.call1((length,)).unwrap();
                compile_act_schema(&schema).unwrap();
            }
            let error = compile_act_schema(&depth.call1((32,)).unwrap()).unwrap_err();
            assert!(error.to_string().contains("depth limit"));

            let nodes = locals.get_item("schema_for_nodes").unwrap().unwrap();
            for lengths in [[vec![31; 32], vec![30]].concat(), vec![31; 33]] {
                let schema = nodes.call1((lengths,)).unwrap();
                compile_act_schema(&schema).unwrap();
            }
            let over_nodes = [vec![31; 33], vec![1]].concat();
            let error = compile_act_schema(&nodes.call1((over_nodes,)).unwrap()).unwrap_err();
            assert!(error.to_string().contains("node limit"));

            let fields = locals.get_item("schema_for_fields").unwrap().unwrap();
            for count in [511, 512] {
                let schema = fields.call1((count,)).unwrap();
                compile_act_schema(&schema).unwrap();
            }
            let error = compile_act_schema(&fields.call1((513,)).unwrap()).unwrap_err();
            assert!(error.to_string().contains("field limit"));

            let choice_counts = locals
                .get_item("schema_for_choice_counts")
                .unwrap()
                .unwrap();
            for counts in [[vec![256; 15], vec![255]].concat(), vec![256; 16]] {
                let schema = choice_counts.call1((counts,)).unwrap();
                compile_act_schema(&schema).unwrap();
            }
            let over_choices = [vec![256; 16], vec![1]].concat();
            let error =
                compile_act_schema(&choice_counts.call1((over_choices,)).unwrap()).unwrap_err();
            assert!(error.to_string().contains("choices"));

            let choice_bytes = locals.get_item("schema_for_choice_bytes").unwrap().unwrap();
            for length in [
                super::SCHEMA_MAX_CHOICE_BYTES - 3,
                super::SCHEMA_MAX_CHOICE_BYTES - 2,
            ] {
                let schema = choice_bytes.call1((length,)).unwrap();
                compile_act_schema(&schema).unwrap();
            }
            let error = compile_act_schema(
                &choice_bytes
                    .call1((super::SCHEMA_MAX_CHOICE_BYTES - 1,))
                    .unwrap(),
            )
            .unwrap_err();
            assert!(error.to_string().contains("choices"));

            let prompt_schema = locals.get_item("schema_for_prompt").unwrap().unwrap();
            let base = compile_act_schema(&prompt_schema.call1((0,)).unwrap()).unwrap();
            let base_length = base.render_prompt("").unwrap().len();
            let exact_name_length = super::PROMPT_MAX_BYTES - base_length;
            for expected in [super::PROMPT_MAX_BYTES - 1, super::PROMPT_MAX_BYTES] {
                let name_length = exact_name_length - (super::PROMPT_MAX_BYTES - expected);
                let compiled =
                    compile_act_schema(&prompt_schema.call1((name_length,)).unwrap()).unwrap();
                assert_eq!(compiled.render_prompt("").unwrap().len(), expected);
            }
            let compiled =
                compile_act_schema(&prompt_schema.call1((exact_name_length + 1,)).unwrap())
                    .unwrap();
            let error = compiled.render_prompt("").unwrap_err();
            assert!(error.to_string().contains("rendered prompt"));
        });
    }

    #[test]
    fn result_depth_nodes_and_strings_enforce_every_exact_boundary() {
        let _guard = crate::initialize_python_for_test();
        Python::attach(|py| {
            let locals = PyDict::new(py);
            locals.set_item("act_schema", module(py)).unwrap();
            let source = CString::new(
                r#"class CustomValue(act_schema.SchemaValue):
    def __init__(self, kind):
        super().__init__(description=f'custom {kind}', json_kind=kind)
    def render_prompt(self):
        return 'must satisfy the global result resources'
    def validate(self, value):
        return None

schemas = {
    kind: {'value': CustomValue(kind)}
    for kind in ('string', 'array', 'object')
}
"#,
            )
            .unwrap();
            py.run(source.as_c_str(), Some(&locals), Some(&locals))
                .unwrap();
            let schemas = locals
                .get_item("schemas")
                .unwrap()
                .unwrap()
                .cast_into::<PyDict>()
                .unwrap();
            let compiled =
                |kind: &str| compile_act_schema(&schemas.get_item(kind).unwrap().unwrap()).unwrap();
            let string_at_utf8_bytes = |bytes: usize| {
                assert_eq!(
                    super::RESULT_STRING_MAX_BYTES,
                    super::RESULT_STRING_MAX_SCALARS * 4
                );
                match bytes {
                    value if value == super::RESULT_STRING_MAX_BYTES - 1 => {
                        let mut text = "\u{1f642}".repeat(super::RESULT_STRING_MAX_SCALARS - 1);
                        text.push('\u{20ac}');
                        text
                    }
                    value if value == super::RESULT_STRING_MAX_BYTES => {
                        "\u{1f642}".repeat(super::RESULT_STRING_MAX_SCALARS)
                    }
                    value if value == super::RESULT_STRING_MAX_BYTES + 1 => {
                        let mut text = "\u{1f642}".repeat(super::RESULT_STRING_MAX_SCALARS);
                        text.push('x');
                        text
                    }
                    _ => panic!("test helper only constructs the UTF-8 byte boundary"),
                }
            };

            let array_schema = compiled("array");
            let nested_result = |depth: usize| {
                let mut nested = json!([]);
                for _ in 2..depth {
                    nested = json!([nested]);
                }
                json!({"value": nested})
            };
            for depth in [31, 32] {
                let NativeValidationOutcome::Valid { custom_jobs, .. } =
                    array_schema.validate(&nested_result(depth))
                else {
                    panic!("result at or below the depth boundary was rejected");
                };
                assert_eq!(custom_jobs.len(), 1);
            }
            let NativeValidationOutcome::Invalid { issues, .. } =
                array_schema.validate(&nested_result(33))
            else {
                panic!("result above the depth boundary was accepted");
            };
            assert!(issues.iter().any(|issue| issue.code == "resource_limit"));

            let object_schema = compiled("object");
            let object_result = |total_nodes: usize| {
                let children = total_nodes - 2;
                let value = (0..children)
                    .map(|index| (format!("field_{index}"), json!(index)))
                    .collect::<serde_json::Map<_, _>>();
                json!({"value": value})
            };
            for total_nodes in [super::RESULT_MAX_NODES - 1, super::RESULT_MAX_NODES] {
                let NativeValidationOutcome::Valid { custom_jobs, .. } =
                    object_schema.validate(&object_result(total_nodes))
                else {
                    panic!("result at or below the node boundary was rejected");
                };
                assert_eq!(custom_jobs.len(), 1);
            }
            let NativeValidationOutcome::Invalid { issues, .. } =
                object_schema.validate(&object_result(super::RESULT_MAX_NODES + 1))
            else {
                panic!("result above the node boundary was accepted");
            };
            assert!(issues.iter().any(|issue| issue.code == "resource_limit"));

            let string_schema = compiled("string");
            for length in [
                super::RESULT_STRING_MAX_SCALARS - 1,
                super::RESULT_STRING_MAX_SCALARS,
            ] {
                let value = json!({"value": "x".repeat(length)});
                assert!(matches!(
                    string_schema.validate(&value),
                    NativeValidationOutcome::Valid { .. }
                ));
            }
            let value = json!({
                "value": "x".repeat(super::RESULT_STRING_MAX_SCALARS + 1)
            });
            assert!(matches!(
                string_schema.validate(&value),
                NativeValidationOutcome::Invalid { .. }
            ));

            for bytes in [
                super::RESULT_STRING_MAX_BYTES - 1,
                super::RESULT_STRING_MAX_BYTES,
            ] {
                let string = string_at_utf8_bytes(bytes);
                assert_eq!(string.len(), bytes);
                assert_eq!(string.chars().count(), super::RESULT_STRING_MAX_SCALARS);
                let value = json!({"value": string});
                assert!(matches!(
                    string_schema.validate(&value),
                    NativeValidationOutcome::Valid { .. }
                ));
            }
            let string = string_at_utf8_bytes(super::RESULT_STRING_MAX_BYTES + 1);
            assert_eq!(string.len(), super::RESULT_STRING_MAX_BYTES + 1);
            let value = json!({"value": string});
            assert!(matches!(
                string_schema.validate(&value),
                NativeValidationOutcome::Invalid { .. }
            ));

            let key_result = |key: String| {
                let mut object = serde_json::Map::new();
                object.insert(key, json!(1));
                json!({"value": object})
            };
            for length in [
                super::RESULT_STRING_MAX_SCALARS - 1,
                super::RESULT_STRING_MAX_SCALARS,
            ] {
                assert!(matches!(
                    object_schema.validate(&key_result("x".repeat(length))),
                    NativeValidationOutcome::Valid { .. }
                ));
            }
            assert!(matches!(
                object_schema.validate(&key_result(
                    "x".repeat(super::RESULT_STRING_MAX_SCALARS + 1)
                )),
                NativeValidationOutcome::Invalid { .. }
            ));
            for bytes in [
                super::RESULT_STRING_MAX_BYTES - 1,
                super::RESULT_STRING_MAX_BYTES,
            ] {
                let key = string_at_utf8_bytes(bytes);
                assert_eq!(key.len(), bytes);
                assert!(matches!(
                    object_schema.validate(&key_result(key)),
                    NativeValidationOutcome::Valid { .. }
                ));
            }
            let key = string_at_utf8_bytes(super::RESULT_STRING_MAX_BYTES + 1);
            assert_eq!(key.len(), super::RESULT_STRING_MAX_BYTES + 1);
            assert!(matches!(
                object_schema.validate(&key_result(key)),
                NativeValidationOutcome::Invalid { .. }
            ));
        });
    }

    #[test]
    fn compiler_rejects_nonexact_roots_builtin_subclasses_and_cycles() {
        let _guard = crate::initialize_python_for_test();
        Python::attach(|py| {
            let locals = PyDict::new(py);
            locals.set_item("act_schema", module(py)).unwrap();
            py.run(
                c"class DictSubclass(dict): pass\n\
                  class StringSubclass(act_schema.StrValue): pass\n\
                  root_subclass = DictSubclass(value=act_schema.StrValue(description='value'))\n\
                  builtin_subclass = {'value': StringSubclass(description='value')}\n\
                  cyclic = act_schema.ListValue(act_schema.StrValue(description='item'), description='cycle')\n\
                  object.__setattr__(cyclic, '_item', cyclic)\n\
                  cycle_schema = {'value': cyclic}",
                None,
                Some(&locals),
            )
            .unwrap();

            for (name, message) in [
                ("root_subclass", "exact dict"),
                ("builtin_subclass", "built-in"),
                ("cycle_schema", "cycle"),
            ] {
                let error =
                    compile_act_schema(&locals.get_item(name).unwrap().unwrap()).unwrap_err();
                assert!(error.to_string().contains(message), "{name}: {error}");
            }
        });
    }

    #[test]
    fn compiler_revalidates_every_builtin_snapshot_invariant() {
        let _guard = crate::initialize_python_for_test();
        Python::attach(|py| {
            let locals = PyDict::new(py);
            locals.set_item("act_schema", module(py)).unwrap();
            let source = CString::new(
                r#"text_description = act_schema.StrValue(description='text')
object.__setattr__(text_description, '_description', ' ')

text_kind = act_schema.StrValue(description='text')
object.__setattr__(text_kind, '_json_kind', 'int64')

text_bounds = act_schema.StrValue(description='text', min_length=1, max_length=2)
object.__setattr__(text_bounds, '_min_length', 3)

integer_choices = act_schema.Int64Value(description='integer', choices=(1, 2))
object.__setattr__(integer_choices, '_choices', (1, 1))

float_bound = act_schema.Float64Value(description='float', min=0.0)
object.__setattr__(float_bound, '_min', float('nan'))

bool_choices = act_schema.BoolValue(description='bool', choices=(True,))
object.__setattr__(bool_choices, '_choices', ())

list_bounds = act_schema.ListValue(
    act_schema.StrValue(description='item'),
    description='items',
    min_items=1,
    max_items=2,
)
object.__setattr__(list_bounds, '_max_items', 0)

object_fields = act_schema.ObjectValue(
    description='object',
    fields={'value': act_schema.StrValue(description='value')},
)
field_value = act_schema.StrValue(description='value')
object.__setattr__(
    object_fields,
    '_fields',
    (('value', field_value, True), ('value', field_value, False)),
)

nullable_description = act_schema.NullableValue(
    act_schema.StrValue(description='nullable'),
)
object.__setattr__(nullable_description, '_description', ' ')

nullable_kind = act_schema.NullableValue(
    act_schema.StrValue(description='nullable'),
)
object.__setattr__(nullable_kind, '_json_kind', 'object')

invalid = (
    text_description,
    text_kind,
    text_bounds,
    integer_choices,
    float_bound,
    bool_choices,
    list_bounds,
    object_fields,
    nullable_description,
    nullable_kind,
)
"#,
            )
            .unwrap();
            py.run(source.as_c_str(), Some(&locals), Some(&locals))
                .unwrap();
            let invalid = locals
                .get_item("invalid")
                .unwrap()
                .unwrap()
                .cast_into::<PyTuple>()
                .unwrap();

            for value in invalid.iter() {
                let schema = PyDict::new(py);
                schema.set_item("value", value).unwrap();
                assert!(compile_act_schema(&schema).is_err());
            }
        });
    }
}
