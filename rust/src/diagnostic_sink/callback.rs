use std::ffi::CStr;

use pyo3::prelude::*;
use pyo3::types::PyAnyMethods;

const EXCEPTION_TYPE_MAX_BYTES: usize = 256;
const EXCEPTION_MESSAGE_MAX_BYTES: usize = 4 * 1024;

const DRIVER_SOURCE: &CStr = cr#"
import asyncio
import contextvars
import inspect
import threading


_RETRY = 0
_DONE = 1
_STOP = 2
_POLL_SECONDS = 0.001


async def _finish_success(bridge, sink_id, sequence):
    while True:
        status = bridge._complete_success(sink_id, sequence)
        if status == _RETRY:
            await asyncio.sleep(0)
            continue
        return status


async def _finish_failed(bridge, sink_id):
    while True:
        status = bridge._complete_failed(sink_id)
        if status == _RETRY:
            await asyncio.sleep(0)
            continue
        return status


async def _drive_sink(bridge, sink_id):
    while True:
        try:
            dispatch = bridge._next_dispatch(sink_id)
            if dispatch is None:
                if bridge._sink_stopped(sink_id) or bridge._stopping():
                    return
                await asyncio.sleep(_POLL_SECONDS)
                continue

            callback, event, sequence = dispatch
            result = callback(event)
            if inspect.isawaitable(result):
                result = await result
            if result is not None:
                bridge._record_invalid_return(sink_id, sequence)
                await _finish_failed(bridge, sink_id)
                return

            if await _finish_success(bridge, sink_id, sequence) == _STOP:
                return
        except asyncio.CancelledError as error:
            if bridge._runtime_cancel_requested(sink_id):
                await _finish_failed(bridge, sink_id)
                return
            bridge._record_raised(sink_id, sequence, error)
            await _finish_failed(bridge, sink_id)
            return
        except BaseException as error:
            bridge._record_raised(sink_id, sequence, error)
            await _finish_failed(bridge, sink_id)
            return


async def _dispatcher_main(bridge):
    tasks = {}
    cancel_requested = False
    while True:
        new_sinks, stopping, cancel_now = bridge._poll_commands()
        for sink_id in new_sinks:
            tasks[sink_id] = contextvars.Context().run(
                asyncio.create_task,
                _drive_sink(bridge, sink_id),
            )
        if cancel_now and not cancel_requested:
            cancel_requested = True
            for sink_id, task in tasks.items():
                if not task.done():
                    bridge._request_runtime_cancel(sink_id)
                    task.cancel()
        if stopping and all(task.done() for task in tasks.values()):
            return
        await asyncio.sleep(_POLL_SECONDS)


def _run_dispatcher(bridge):
    loop = asyncio.new_event_loop()
    asyncio.set_event_loop(loop)
    bridge._ready(
        threading.get_ident(),
        id(loop),
        threading.current_thread().daemon,
    )
    try:
        contextvars.Context().run(
            loop.run_until_complete,
            _dispatcher_main(bridge),
        )
    finally:
        asyncio.set_event_loop(None)
        loop.close()
"#;

pub(crate) const fn driver_source() -> &'static CStr {
    DRIVER_SOURCE
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CallbackFailureKind {
    Raised,
    InvalidReturn,
}

impl CallbackFailureKind {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Raised => "raised",
            Self::InvalidReturn => "invalid_return",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CallbackFailure {
    kind: CallbackFailureKind,
    event_sequence: u64,
    exception_type: Option<String>,
    message: Option<String>,
    message_truncated: bool,
}

impl CallbackFailure {
    pub(crate) fn raised(event_sequence: u64, exception: &Bound<'_, PyAny>) -> Self {
        let exception_type = exception
            .get_type()
            .getattr("__qualname__")
            .and_then(|name| name.extract::<String>())
            .unwrap_or_else(|_| "BaseException".to_owned());
        let (exception_type, _) = truncate_utf8(exception_type, EXCEPTION_TYPE_MAX_BYTES);
        let message = exception
            .str()
            .ok()
            .and_then(|value| value.to_str().ok().map(ToOwned::to_owned));
        let (message, message_truncated) = match message {
            Some(message) => {
                let (message, truncated) = truncate_utf8(message, EXCEPTION_MESSAGE_MAX_BYTES);
                (Some(message), truncated)
            }
            None => (None, false),
        };
        Self {
            kind: CallbackFailureKind::Raised,
            event_sequence,
            exception_type: Some(exception_type),
            message,
            message_truncated,
        }
    }

    pub(crate) const fn invalid_return(event_sequence: u64) -> Self {
        Self {
            kind: CallbackFailureKind::InvalidReturn,
            event_sequence,
            exception_type: None,
            message: None,
            message_truncated: false,
        }
    }

    pub(crate) const fn kind(&self) -> CallbackFailureKind {
        self.kind
    }

    pub(crate) const fn event_sequence(&self) -> u64 {
        self.event_sequence
    }

    pub(crate) fn exception_type(&self) -> Option<&str> {
        self.exception_type.as_deref()
    }

    pub(crate) fn message(&self) -> Option<&str> {
        self.message.as_deref()
    }

    pub(crate) const fn message_truncated(&self) -> bool {
        self.message_truncated
    }
}

fn truncate_utf8(mut value: String, max_bytes: usize) -> (String, bool) {
    if value.len() <= max_bytes {
        return (value, false);
    }
    let mut boundary = max_bytes;
    while !value.is_char_boundary(boundary) {
        boundary -= 1;
    }
    value.truncate(boundary);
    (value, true)
}
