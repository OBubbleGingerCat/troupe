use std::ffi::CStr;

const SOURCE: &CStr = cr#"
import asyncio as _asyncio
import threading as _threading
from abc import ABC as _ABC, abstractmethod as _abstractmethod
from collections.abc import Awaitable as _Awaitable


_CAPTURE_FIELDS = (
    "agent_messages",
    "plans",
    "tool_calls",
    "result_validation",
    "usage",
    "custom_events",
    "tool_inputs",
    "tool_outputs",
)
_SINK_STATES = frozenset(("UNBOUND", "BOUND", "SEALED", "CLOSED"))
_SINK_STATE_ERROR_CODES = frozenset(("uninitialized", "unbound", "already_bound"))
_CALLBACK_FAILURE_KINDS = frozenset(("raised", "invalid_return"))
_ACT_OUTCOMES = frozenset(("completed", "cancelled", "failed"))
_SINK_CLOSE_REASONS = frozenset((
    "act_finished",
    "callback_failed",
    "delivery_overflow",
    "runtime_shutdown",
))


def _require_optional_enum(value, allowed, field):
    if value is not None:
        _require_enum(value, allowed, field)
    return value


@_final
@_dataclass(frozen=True, slots=True, kw_only=True)
class DiagnosticCapture:
    agent_messages: bool = True
    plans: bool = True
    tool_calls: bool = True
    result_validation: bool = True
    usage: bool = True
    custom_events: bool = True
    tool_inputs: bool = False
    tool_outputs: bool = False

    def __post_init__(self):
        for field in _CAPTURE_FIELDS:
            _require_bool(getattr(self, field), field)
        if not self.tool_calls and (self.tool_inputs or self.tool_outputs):
            raise ValueError("tool_inputs and tool_outputs require tool_calls")


@_final
class DiagnosticSinkStateError(RuntimeError):
    __slots__ = ("_code",)

    def __init__(self, *, code):
        _require_enum(code, _SINK_STATE_ERROR_CODES, "code")
        super().__init__(code)
        self._code = code

    @property
    def code(self):
        return self._code


@_final
@_dataclass(frozen=True, slots=True, kw_only=True)
class DiagnosticCallbackFailure:
    kind: str
    event_sequence: int
    exception_type: str | None
    message: str | None
    message_truncated: bool

    def __post_init__(self):
        _require_enum(self.kind, _CALLBACK_FAILURE_KINDS, "kind")
        _require_u64(self.event_sequence, "event_sequence", nonzero=True)
        _require_optional_string(self.exception_type, "exception_type")
        _require_optional_string(self.message, "message")
        _require_bool(self.message_truncated, "message_truncated")
        if self.kind == "raised":
            if not self.exception_type:
                raise ValueError("raised callback failure requires exception_type")
        elif self.exception_type is not None or self.message is not None:
            raise ValueError("invalid_return callback failure cannot carry exception metadata")


@_final
@_dataclass(frozen=True, slots=True, kw_only=True)
class DiagnosticDropCount:
    event_kind: str
    events: int
    encoded_bytes: int

    def __post_init__(self):
        _require_enum(self.event_kind, _EVENT_KINDS, "event_kind")
        _require_u64(self.events, "events")
        _require_u64(self.encoded_bytes, "encoded_bytes")


@_final
@_dataclass(frozen=True, slots=True, kw_only=True)
class DiagnosticSinkSummary:
    run_id: _UUID
    act_id: str
    act_outcome: str | None
    close_reason: str
    complete: bool
    delivered_events: int
    first_delivered_sequence: int | None
    last_delivered_sequence: int | None
    dropped_events: int
    dropped_bytes: int
    dropped_by_kind: tuple[DiagnosticDropCount, ...]
    source_gaps: int
    truncated_payloads: int
    callback_failure: DiagnosticCallbackFailure | None
    callback_abandoned: bool

    def __post_init__(self):
        _require_uuid(self.run_id, "run_id")
        _require_run_local_id(self.act_id, "act_id")
        _require_optional_enum(self.act_outcome, _ACT_OUTCOMES, "act_outcome")
        _require_enum(self.close_reason, _SINK_CLOSE_REASONS, "close_reason")
        _require_bool(self.complete, "complete")
        _require_u64(self.delivered_events, "delivered_events")
        _require_optional_u64(
            self.first_delivered_sequence,
            "first_delivered_sequence",
            nonzero=True,
        )
        _require_optional_u64(
            self.last_delivered_sequence,
            "last_delivered_sequence",
            nonzero=True,
        )
        _require_u64(self.dropped_events, "dropped_events")
        _require_u64(self.dropped_bytes, "dropped_bytes")
        if type(self.dropped_by_kind) is not tuple or any(
            type(count) is not DiagnosticDropCount for count in self.dropped_by_kind
        ):
            raise TypeError("dropped_by_kind must be a tuple of exact DiagnosticDropCount values")
        drop_kinds = tuple(count.event_kind for count in self.dropped_by_kind)
        if len(drop_kinds) != len(set(drop_kinds)):
            raise ValueError("dropped_by_kind event kinds must be unique")
        if sum(count.events for count in self.dropped_by_kind) != self.dropped_events:
            raise ValueError("dropped_events must equal dropped_by_kind event counts")
        if sum(count.encoded_bytes for count in self.dropped_by_kind) != self.dropped_bytes:
            raise ValueError("dropped_bytes must equal dropped_by_kind encoded bytes")
        _require_u64(self.source_gaps, "source_gaps")
        _require_u64(self.truncated_payloads, "truncated_payloads")
        if self.callback_failure is not None and type(self.callback_failure) is not DiagnosticCallbackFailure:
            raise TypeError("callback_failure must be an exact DiagnosticCallbackFailure or None")
        _require_bool(self.callback_abandoned, "callback_abandoned")

        delivered_sequences = (
            self.first_delivered_sequence,
            self.last_delivered_sequence,
        )
        if self.delivered_events == 0:
            if delivered_sequences != (None, None):
                raise ValueError("an empty delivery range must not contain sequences")
        elif None in delivered_sequences:
            raise ValueError("a nonempty delivery range requires both endpoint sequences")
        elif self.first_delivered_sequence > self.last_delivered_sequence:
            raise ValueError("the delivery sequence range is reversed")

        if (self.close_reason == "callback_failed") != (self.callback_failure is not None):
            raise ValueError("callback_failed close reason and callback_failure must agree")
        if self.callback_abandoned and self.close_reason != "runtime_shutdown":
            raise ValueError("callback_abandoned requires runtime_shutdown")
        if self.complete and (
            self.close_reason != "act_finished"
            or self.dropped_events != 0
            or self.source_gaps != 0
            or self.truncated_payloads != 0
            or self.callback_failure is not None
            or self.callback_abandoned
        ):
            raise ValueError("complete conflicts with delivery incompleteness facts")


class DiagnosticSink(_ABC):
    __slots__ = ("__capture", "__lock", "__state", "__summary", "__waiters")

    def __init__(self, *, capture: DiagnosticCapture | None = None) -> None:
        try:
            object.__getattribute__(self, "_DiagnosticSink__state")
        except AttributeError:
            pass
        else:
            raise DiagnosticSinkStateError(code="already_bound")
        if capture is None:
            capture = DiagnosticCapture()
        elif type(capture) is not DiagnosticCapture:
            raise TypeError("capture must be an exact DiagnosticCapture or None")
        self.__capture = capture
        self.__lock = _threading.RLock()
        self.__state = "UNBOUND"
        self.__summary = None
        self.__waiters = []

    def _diagnostic_require_lock(self):
        try:
            return object.__getattribute__(self, "_DiagnosticSink__lock")
        except AttributeError as error:
            raise DiagnosticSinkStateError(code="uninitialized") from error

    def _diagnostic_require_state(self):
        lock = self._diagnostic_require_lock()
        with lock:
            state = object.__getattribute__(self, "_DiagnosticSink__state")
        if state not in _SINK_STATES:
            raise RuntimeError("diagnostic sink lifecycle state is corrupt")
        return state

    @property
    def capture(self):
        self._diagnostic_require_state()
        return object.__getattribute__(self, "_DiagnosticSink__capture")

    @property
    def state(self):
        return self._diagnostic_require_state()

    @_abstractmethod
    def on_event(self, event: DiagnosticEvent, /) -> None | _Awaitable[None]:
        raise NotImplementedError

    async def wait_closed(self) -> DiagnosticSinkSummary:
        lock = self._diagnostic_require_lock()
        loop = _asyncio.get_running_loop()
        waiter = loop.create_future()

        def publish_result(summary):
            if not waiter.done():
                waiter.set_result(summary)

        def completion_finished(summary):
            try:
                loop.call_soon_threadsafe(publish_result, summary)
            except RuntimeError:
                # A cancelled waiter may already have let its event loop close.
                pass

        with lock:
            state = object.__getattribute__(self, "_DiagnosticSink__state")
            if state == "UNBOUND":
                raise DiagnosticSinkStateError(code="unbound")
            if state == "CLOSED":
                return object.__getattribute__(self, "_DiagnosticSink__summary")
            waiters = object.__getattribute__(self, "_DiagnosticSink__waiters")
            waiters.append(completion_finished)
        try:
            return await waiter
        finally:
            with lock:
                try:
                    waiters.remove(completion_finished)
                except ValueError:
                    pass

    def _diagnostic_bind(self):
        lock = self._diagnostic_require_lock()
        with lock:
            state = object.__getattribute__(self, "_DiagnosticSink__state")
            if state != "UNBOUND":
                raise DiagnosticSinkStateError(code="already_bound")
            self.__state = "BOUND"

    def _diagnostic_seal(self):
        lock = self._diagnostic_require_lock()
        with lock:
            state = object.__getattribute__(self, "_DiagnosticSink__state")
            if state == "UNBOUND":
                raise DiagnosticSinkStateError(code="unbound")
            if state != "BOUND":
                raise RuntimeError("diagnostic sink can only be sealed from BOUND")
            self.__state = "SEALED"

    def _diagnostic_close(self, summary):
        if type(summary) is not DiagnosticSinkSummary:
            raise TypeError("summary must be an exact DiagnosticSinkSummary")
        lock = self._diagnostic_require_lock()
        with lock:
            state = object.__getattribute__(self, "_DiagnosticSink__state")
            if state == "UNBOUND":
                raise DiagnosticSinkStateError(code="unbound")
            if state != "SEALED":
                raise RuntimeError("diagnostic sink can only be closed from SEALED")
            self.__state = "CLOSED"
            self.__summary = summary
            waiters = object.__getattribute__(self, "_DiagnosticSink__waiters")
            pending = tuple(waiters)
            waiters.clear()
        for waiter in pending:
            waiter(summary)
"#;

pub(crate) const fn source() -> &'static CStr {
    SOURCE
}
