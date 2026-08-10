from __future__ import annotations

from abc import ABC, abstractmethod
from collections.abc import Awaitable, Sequence
from typing import Any, Generic, Literal, TypeAlias, TypeVar, final

_JsonKind: TypeAlias = Literal["string", "int64", "float64", "bool", "array", "object"]
_JsonValue: TypeAlias = (
    None | bool | int | float | str | list["_JsonValue"] | dict[str, "_JsonValue"]
)
_SchemaValueT = TypeVar("_SchemaValueT")
_ItemT = TypeVar("_ItemT")
_ValueT = TypeVar("_ValueT")

class ValueRejected(ValueError):
    def __init__(self, message: str) -> None: ...

class SchemaCallbackError(RuntimeError):
    @property
    def phase(self) -> Literal["render_prompt", "validate"]: ...
    @property
    def path(self) -> str: ...
    def __init__(
        self,
        message: str,
        *,
        phase: Literal["render_prompt", "validate"],
        path: str,
    ) -> None: ...

class SchemaValue(Generic[_SchemaValueT], ABC):
    def __init__(self, *, description: str, json_kind: _JsonKind) -> None: ...
    @property
    def description(self) -> str: ...
    @property
    def json_kind(self) -> _JsonKind: ...
    @abstractmethod
    def render_prompt(self) -> str: ...
    @abstractmethod
    def validate(
        self,
        value: _SchemaValueT,
        /,
    ) -> None | Awaitable[None]: ...

@final
class StrValue(SchemaValue[str]):
    def __init__(
        self,
        *,
        description: str,
        min_length: int | None = None,
        max_length: int | None = None,
        choices: Sequence[str] | None = None,
    ) -> None: ...
    def render_prompt(self) -> str: ...
    def validate(self, value: str, /) -> None: ...

@final
class Int64Value(SchemaValue[int]):
    def __init__(
        self,
        *,
        description: str,
        min: int | None = None,
        max: int | None = None,
        choices: Sequence[int] | None = None,
    ) -> None: ...
    def render_prompt(self) -> str: ...
    def validate(self, value: int, /) -> None: ...

@final
class Float64Value(SchemaValue[float]):
    def __init__(
        self,
        *,
        description: str,
        min: float | None = None,
        max: float | None = None,
        choices: Sequence[float] | None = None,
    ) -> None: ...
    def render_prompt(self) -> str: ...
    def validate(self, value: float, /) -> None: ...

@final
class BoolValue(SchemaValue[bool]):
    def __init__(
        self,
        *,
        description: str,
        choices: Sequence[bool] | None = None,
    ) -> None: ...
    def render_prompt(self) -> str: ...
    def validate(self, value: bool, /) -> None: ...

@final
class ListValue(SchemaValue[list[_ItemT]], Generic[_ItemT]):
    def __init__(
        self,
        item: SchemaValue[_ItemT],
        *,
        description: str,
        min_items: int | None = None,
        max_items: int | None = None,
    ) -> None: ...
    def render_prompt(self) -> str: ...
    def validate(self, value: list[_ItemT], /) -> None | Awaitable[None]: ...

@final
class ObjectValue(SchemaValue[dict[str, _JsonValue]]):
    def __init__(
        self,
        *,
        description: str,
        fields: dict[str, FieldSpec],
    ) -> None: ...
    def render_prompt(self) -> str: ...
    def validate(self, value: dict[str, _JsonValue], /) -> None | Awaitable[None]: ...

@final
class NullableValue(SchemaValue[_ValueT | None], Generic[_ValueT]):
    def __init__(self, inner: SchemaValue[_ValueT]) -> None: ...
    def render_prompt(self) -> str: ...
    def validate(self, value: _ValueT | None, /) -> None | Awaitable[None]: ...

@final
class Field(Generic[_ValueT]):
    def __init__(self, inner: SchemaValue[_ValueT], *, required: bool) -> None: ...

FieldSpec: TypeAlias = SchemaValue[Any] | Field[Any]

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
