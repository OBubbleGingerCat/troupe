from __future__ import annotations

import asyncio
import argparse
import json
import os
import threading
from importlib.resources import files

import recording_production.config as absolute_config
from troupe import Production as BaseProduction

from . import config as relative_config
from .workers import WORKER_VALUE


construction_count = 0


class CleanupBoom(Exception):
    pass


class StartBoom(Exception):
    pass


class SceneBoom(Exception):
    pass


class StopBoom(Exception):
    pass


class Production(BaseProduction):
    def __init__(self, args: list[str]) -> None:
        global construction_count
        construction_count += 1
        self.received = args
        self.events_path: str | None = None
        self.events_fd: int | None = None
        self.control_fd: int | None = None
        self._control_buffer = bytearray()
        self._control_commands: list[bytes] = []

        remaining = list(args)
        if remaining and remaining[0] in {"--help", "--bad-option"}:
            parser = argparse.ArgumentParser(prog="recording-production")
            parser.parse_args(remaining)
        if remaining[:1] == ["--events"]:
            if len(remaining) < 2:
                raise ValueError("--events requires a path")
            self.events_path = remaining[1]
            remaining = remaining[2:]
        elif remaining[:1] == ["--events-fd"]:
            if len(remaining) < 3:
                raise ValueError("--events-fd requires event and control fds")
            self.events_fd = int(remaining[1])
            self.control_fd = int(remaining[2])
            os.set_blocking(self.control_fd, False)
            remaining = remaining[3:]

        if len(remaining) == 1 and os.fsencode(remaining[0]) == b"\xff":
            self.mode = "surrogate-argv"
            self.mode_args = remaining
        else:
            self.mode = remaining[0] if remaining else "serial"
            self.mode_args = remaining[1:]
        self.relative_config = relative_config
        self.absolute_config = absolute_config
        self.config_value = relative_config.CONFIG_VALUE
        self.worker_value = WORKER_VALUE
        self.resource_value = (
            files("recording_production")
            .joinpath("resources", "marker.txt")
            .read_text(encoding="utf-8")
            .strip()
        )
        self.lifecycle_events: list[str] = []
        self.active_hooks = 0
        self.max_active_hooks = 0
        self.stop_entry_active_hooks: list[int] = []
        self.scene_count = 0
        self.start_entered = asyncio.Event()
        self.start_release = asyncio.Event()
        self.scene_entered = asyncio.Event()
        self.scene_blocker = asyncio.Event()
        self.scene_finally = asyncio.Event()
        self.cancel_caught = asyncio.Event()
        self.cleanup_release = asyncio.Event()
        self.cancel_count = 0
        self.scene_task: asyncio.Task[None] | None = None
        self.cleanup_error = CleanupBoom("scene cancellation cleanup failed")
        self.scene_three_entered = asyncio.Event()
        self.scene_three_release = asyncio.Event()
        self.stop_entered = asyncio.Event()
        self.stop_release = asyncio.Event()

    def _emit(self, event: str, **values: object) -> None:
        record = {"event": event, **values}
        payload = (json.dumps(record, sort_keys=True) + "\n").encode("utf-8")
        if self.events_fd is not None:
            os.write(self.events_fd, payload)
        elif self.events_path is not None:
            with open(self.events_path, "ab") as stream:
                stream.write(payload)

    async def _next_control_command(self) -> bytes:
        if self._control_commands:
            return self._control_commands.pop(0)
        if self.control_fd is None:
            raise RuntimeError("control fd is not configured")

        loop = asyncio.get_running_loop()
        command_ready: asyncio.Future[bytes] = loop.create_future()

        def read_control() -> None:
            try:
                chunk = os.read(self.control_fd, 4096)
                if not chunk:
                    raise EOFError("control pipe closed")
                self._control_buffer.extend(chunk)
                while b"\n" in self._control_buffer:
                    command, _, rest = self._control_buffer.partition(b"\n")
                    self._control_buffer[:] = rest
                    self._control_commands.append(command)
                if self._control_commands and not command_ready.done():
                    command_ready.set_result(self._control_commands.pop(0))
            except BaseException as error:
                if not command_ready.done():
                    command_ready.set_exception(error)

        loop.add_reader(self.control_fd, read_control)
        try:
            return await command_ready
        finally:
            loop.remove_reader(self.control_fd)

    def _enter_hook(self) -> None:
        self.active_hooks += 1
        self.max_active_hooks = max(self.max_active_hooks, self.active_hooks)

    def _leave_hook(self) -> None:
        self.active_hooks -= 1

    async def start(self) -> None:
        self._enter_hook()
        try:
            self.lifecycle_events.append("start")
            if self.mode in {
                "cli-normal",
                "dual-fails",
                "loop-record",
                "raw-args",
                "signal-finally",
                "signal-repeat",
                "start-fails",
                "surrogate-argv",
            }:
                self._emit(
                    "start",
                    thread=threading.get_ident(),
                    loop=id(asyncio.get_running_loop()),
                )
                if self.mode == "raw-args":
                    self._emit("args", args=self.mode_args)
                if self.mode == "surrogate-argv":
                    assert len(self.mode_args) == 1
                    assert type(self.mode_args[0]) is str
                    assert os.fsencode(self.mode_args[0]) == b"\xff"
                if self.mode == "start-fails":
                    raise StartBoom("start marker")
                return
            self.start_entered.set()
            await self.start_release.wait()
        finally:
            self._leave_hook()

    async def scene(self) -> None:
        self._enter_hook()
        try:
            self.scene_count += 1
            if self.mode in {
                "cli-normal",
                "loop-record",
                "raw-args",
                "surrogate-argv",
            }:
                self.lifecycle_events.append("scene")
                self._emit(
                    "scene",
                    thread=threading.get_ident(),
                    loop=id(asyncio.get_running_loop()),
                )
                raise asyncio.CancelledError("fixture completed")
            if self.mode == "dual-fails":
                self.lifecycle_events.append("scene")
                self._emit("scene")
                raise SceneBoom("scene marker")
            if self.mode in {"signal-finally", "signal-repeat"}:
                self.lifecycle_events.append("scene-enter")
                self._emit("scene-enter")
                try:
                    await self.scene_blocker.wait()
                except asyncio.CancelledError:
                    self.cancel_count += 1
                    if self.mode == "signal-repeat":
                        self.lifecycle_events.append("cancel-caught")
                        self._emit("cancel-caught", count=self.cancel_count)
                        command = await self._next_control_command()
                        if command != b"probe":
                            raise RuntimeError(f"unexpected control command: {command!r}")
                        self._emit("probe-ack")
                        command = await self._next_control_command()
                        if command != b"release":
                            raise RuntimeError(f"unexpected control command: {command!r}")
                        self.lifecycle_events.append("scene-cleanup")
                        self._emit("scene-cleanup", count=self.cancel_count)
                        return
                    raise
                finally:
                    if self.mode == "signal-finally":
                        self.lifecycle_events.append("scene-finally")
                        self._emit("scene-finally")
                return
            if self.mode in {"cancel-finally", "swallow-cancel", "cleanup-fails"}:
                self.lifecycle_events.append("scene-enter")
                self.scene_task = asyncio.current_task()
                self.scene_entered.set()
                try:
                    await self.scene_blocker.wait()
                except asyncio.CancelledError:
                    self.cancel_count += 1
                    if self.mode == "swallow-cancel":
                        self.lifecycle_events.append("cancel-caught")
                        self.cancel_caught.set()
                        try:
                            await self.cleanup_release.wait()
                        except asyncio.CancelledError:
                            self.cancel_count += 1
                            raise
                        self.lifecycle_events.append("scene-cleanup")
                        return
                    if self.mode == "cleanup-fails":
                        self.lifecycle_events.append("scene-cleanup")
                        self.cancel_caught.set()
                        raise self.cleanup_error
                    raise
                finally:
                    if self.mode == "cancel-finally":
                        self.lifecycle_events.append("scene-finally")
                    self.scene_finally.set()
                return
            if self.scene_count < 3:
                self.lifecycle_events.append(f"scene:{self.scene_count}")
                return
            self.lifecycle_events.append(f"scene:{self.scene_count}-enter")
            self.scene_three_entered.set()
            try:
                await self.scene_three_release.wait()
            except asyncio.CancelledError:
                await self.scene_three_release.wait()
            self.lifecycle_events.append(f"scene:{self.scene_count}-exit")
        finally:
            self._leave_hook()

    async def stop(self) -> None:
        self.stop_entry_active_hooks.append(self.active_hooks)
        self._enter_hook()
        try:
            self.lifecycle_events.append("stop")
            if self.mode in {
                "cli-normal",
                "dual-fails",
                "loop-record",
                "raw-args",
                "signal-finally",
                "signal-repeat",
                "surrogate-argv",
            }:
                self._emit(
                    "stop",
                    thread=threading.get_ident(),
                    loop=id(asyncio.get_running_loop()),
                )
                if self.mode == "dual-fails":
                    raise StopBoom("stop marker")
                return
            self.stop_entered.set()
            await self.stop_release.wait()
        finally:
            self._leave_hook()
