#!/usr/bin/env python3
"""Independent stdlib decoder for Troupe's closed Perfetto wire subset."""

from __future__ import annotations

import argparse
import hashlib
import json
import math
import re
import struct
import sys
from collections import Counter
from dataclasses import dataclass
from pathlib import Path, PurePosixPath
from typing import Any, Final


TRACE_FIXTURES: Final = {
    "active-watermark",
    "archive-watermark",
    "empty",
    "multi-cue",
    "nested",
    "numeric-boundary",
    "open",
    "overlap",
    "repeated-dump",
}
CONTENT_WARNING: Final = (
    "trace may contain sensitive diagnostic metadata and user-provided attributes"
)
EVENT_TYPES: Final = {
    1: "slice_begin",
    2: "slice_end",
    3: "instant",
    4: "counter",
}
MAX_FIELD_NUMBER: Final = (1 << 29) - 1


class DecodeError(RuntimeError):
    pass


class CompatibilityError(RuntimeError):
    pass


@dataclass(frozen=True)
class WireField:
    number: int
    wire_type: int
    value: int | bytes


class WireReader:
    def __init__(self, data: bytes, context: str) -> None:
        self._data = data
        self._context = context
        self._position = 0

    def _read_varint(self) -> int:
        start = self._position
        value = 0
        for index in range(10):
            if self._position >= len(self._data):
                raise DecodeError(f"{self._context}: truncated varint at byte {start}")
            byte = self._data[self._position]
            self._position += 1
            if index == 9 and byte > 1:
                raise DecodeError(f"{self._context}: varint overflow at byte {start}")
            value |= (byte & 0x7F) << (index * 7)
            if byte < 0x80:
                if self._position - start != _varint_size(value):
                    raise DecodeError(
                        f"{self._context}: non-canonical varint at byte {start}"
                    )
                return value
        raise DecodeError(f"{self._context}: varint overflow at byte {start}")

    def fields(self) -> list[WireField]:
        fields: list[WireField] = []
        while self._position < len(self._data):
            key_offset = self._position
            key = self._read_varint()
            number = key >> 3
            wire_type = key & 7
            if number == 0 or number > MAX_FIELD_NUMBER:
                raise DecodeError(
                    f"{self._context}: invalid field number {number} at byte {key_offset}"
                )
            if wire_type == 0:
                value: int | bytes = self._read_varint()
            elif wire_type == 1:
                value = self._take(8, key_offset)
            elif wire_type == 2:
                length = self._read_varint()
                value = self._take(length, key_offset)
            elif wire_type == 5:
                value = self._take(4, key_offset)
            else:
                raise DecodeError(
                    f"{self._context}: unsupported wire type {wire_type} at byte {key_offset}"
                )
            fields.append(WireField(number, wire_type, value))
        return fields

    def _take(self, length: int, offset: int) -> bytes:
        end = self._position + length
        if end > len(self._data):
            raise DecodeError(
                f"{self._context}: truncated field at byte {offset}; "
                f"need {length} payload bytes"
            )
        value = self._data[self._position : end]
        self._position = end
        return value


def _varint_size(value: int) -> int:
    size = 1
    while value >= 0x80:
        value >>= 7
        size += 1
    return size


def _unknown(field: WireField) -> dict[str, int]:
    return {"number": field.number, "wire_type": field.wire_type}


def _partition(
    data: bytes,
    context: str,
    schema: dict[int, tuple[str, int]],
) -> tuple[dict[int, list[WireField]], list[dict[str, int]]]:
    known = {number: [] for number in schema}
    unknown = []
    for field in WireReader(data, context).fields():
        specification = schema.get(field.number)
        if specification is None:
            unknown.append(_unknown(field))
            continue
        name, expected_wire = specification
        if field.wire_type != expected_wire:
            raise DecodeError(
                f"{context}.{name}: wire type {field.wire_type}, expected {expected_wire}"
            )
        known[field.number].append(field)
    return known, unknown


def _single(
    fields: dict[int, list[WireField]],
    number: int,
    context: str,
    name: str,
    *,
    required: bool,
) -> WireField | None:
    values = fields[number]
    if len(values) > 1:
        raise DecodeError(f"{context}.{name}: duplicate singular field")
    if required and not values:
        raise DecodeError(f"{context}.{name}: missing required field")
    return values[0] if values else None


def _integer(field: WireField) -> int:
    assert isinstance(field.value, int)
    return field.value


def _bytes(field: WireField) -> bytes:
    assert isinstance(field.value, bytes)
    return field.value


def _utf8(field: WireField, context: str) -> str:
    try:
        return _bytes(field).decode("utf-8")
    except UnicodeDecodeError as error:
        raise DecodeError(f"{context}: invalid UTF-8: {error}") from error


def _signed_int64(value: int) -> int:
    return value - (1 << 64) if value >= (1 << 63) else value


def _double(field: WireField, context: str) -> float:
    value = struct.unpack("<d", _bytes(field))[0]
    if not math.isfinite(value):
        raise DecodeError(f"{context}: non-finite double")
    return value


def _decode_annotation(data: bytes, packet_index: int, annotation_index: int) -> dict[str, Any]:
    context = f"Trace.packet[{packet_index}].track_event.debug_annotations[{annotation_index}]"
    fields, unknown = _partition(
        data,
        context,
        {
            2: ("bool_value", 0),
            3: ("uint_value", 0),
            4: ("int_value", 0),
            5: ("double_value", 1),
            6: ("string_value", 2),
            10: ("name", 2),
        },
    )
    name_field = _single(fields, 10, context, "name", required=True)
    assert name_field is not None
    name = _utf8(name_field, f"{context}.name")
    if not name:
        raise DecodeError(f"{context}.name: empty annotation name")

    present = [number for number in (2, 3, 4, 5, 6) if fields[number]]
    if len(present) != 1:
        raise DecodeError(f"{context}.value: expected exactly one value field")
    number = present[0]
    value_field = _single(fields, number, context, "value", required=True)
    assert value_field is not None
    if number == 2:
        raw = _integer(value_field)
        if raw not in (0, 1):
            raise DecodeError(f"{context}.bool_value: expected 0 or 1")
        value_key, value = "bool", bool(raw)
    elif number == 3:
        value_key, value = "uint", str(_integer(value_field))
    elif number == 4:
        value_key, value = "int", str(_signed_int64(_integer(value_field)))
    elif number == 5:
        value_key, value = "double", repr(_double(value_field, f"{context}.double_value"))
    else:
        value_key, value = "string", _utf8(value_field, f"{context}.string_value")
    return {"name": name, value_key: value, "unknown_fields": unknown}


def _decode_track_event(data: bytes, packet_index: int) -> dict[str, Any]:
    context = f"Trace.packet[{packet_index}].track_event"
    fields, unknown = _partition(
        data,
        context,
        {
            4: ("debug_annotations", 2),
            9: ("type", 0),
            11: ("track_uuid", 0),
            23: ("name", 2),
            30: ("counter_value", 0),
            44: ("double_counter_value", 1),
            47: ("flow_ids", 1),
            48: ("terminating_flow_ids", 1),
        },
    )
    type_field = _single(fields, 9, context, "type", required=True)
    track_field = _single(fields, 11, context, "track_uuid", required=True)
    name_field = _single(fields, 23, context, "name", required=False)
    integer_counter = _single(fields, 30, context, "counter_value", required=False)
    double_counter = _single(fields, 44, context, "double_counter_value", required=False)
    assert type_field is not None and track_field is not None

    event_type = EVENT_TYPES.get(_integer(type_field))
    if event_type is None:
        raise DecodeError(f"{context}.type: unknown enum value {_integer(type_field)}")
    track_uuid = _integer(track_field)
    if track_uuid == 0:
        raise DecodeError(f"{context}.track_uuid: zero is not a valid dense identity")
    name = _utf8(name_field, f"{context}.name") if name_field is not None else None
    if event_type != "slice_end" and not name:
        raise DecodeError(f"{context}.name: required for {event_type}")

    if integer_counter is not None and double_counter is not None:
        raise DecodeError(f"{context}.counter_value_field: multiple oneof members")
    counter: dict[str, str] | None = None
    if integer_counter is not None:
        counter = {"int64": str(_signed_int64(_integer(integer_counter)))}
    elif double_counter is not None:
        counter = {"double": repr(_double(double_counter, f"{context}.double_counter_value"))}
    if (event_type == "counter") != (counter is not None):
        raise DecodeError(f"{context}: counter type/value mismatch")

    flow_ids = [str(struct.unpack("<Q", _bytes(field))[0]) for field in fields[47]]
    terminating_flow_ids = [
        str(struct.unpack("<Q", _bytes(field))[0]) for field in fields[48]
    ]
    if any(value == "0" for value in flow_ids + terminating_flow_ids):
        raise DecodeError(f"{context}: flow identity must be nonzero")
    if len(flow_ids) != len(set(flow_ids)) or len(terminating_flow_ids) != len(
        set(terminating_flow_ids)
    ):
        raise DecodeError(f"{context}: duplicate flow identity")

    annotations = [
        _decode_annotation(_bytes(field), packet_index, index)
        for index, field in enumerate(fields[4])
    ]
    return {
        "type": event_type,
        "track_uuid": str(track_uuid),
        "name": name,
        "counter": counter,
        "flow_ids": flow_ids,
        "terminating_flow_ids": terminating_flow_ids,
        "annotations": annotations,
        "unknown_fields": unknown,
    }


def _decode_descriptor(data: bytes, packet_index: int) -> dict[str, Any]:
    context = f"Trace.packet[{packet_index}].track_descriptor"
    fields, unknown = _partition(
        data,
        context,
        {1: ("uuid", 0), 2: ("name", 2), 5: ("parent_uuid", 0)},
    )
    uuid_field = _single(fields, 1, context, "uuid", required=True)
    name_field = _single(fields, 2, context, "name", required=True)
    parent_field = _single(fields, 5, context, "parent_uuid", required=False)
    assert uuid_field is not None and name_field is not None
    uuid = _integer(uuid_field)
    if uuid == 0:
        raise DecodeError(f"{context}.uuid: zero is not a valid dense identity")
    name = _utf8(name_field, f"{context}.name")
    if not name:
        raise DecodeError(f"{context}.name: empty descriptor name")
    parent = _integer(parent_field) if parent_field is not None else None
    if parent == uuid:
        raise DecodeError(f"{context}.parent_uuid: descriptor cannot parent itself")
    return {
        "uuid": str(uuid),
        "name": name,
        "parent_uuid": str(parent) if parent is not None else None,
        "unknown_fields": unknown,
    }


def _decode_packet(data: bytes, packet_index: int) -> dict[str, Any]:
    context = f"Trace.packet[{packet_index}]"
    fields, unknown = _partition(
        data,
        context,
        {
            8: ("timestamp", 0),
            11: ("track_event", 2),
            58: ("timestamp_clock_id", 0),
            60: ("track_descriptor", 2),
        },
    )
    timestamp = _single(fields, 8, context, "timestamp", required=False)
    clock = _single(fields, 58, context, "timestamp_clock_id", required=False)
    event = _single(fields, 11, context, "track_event", required=False)
    descriptor = _single(fields, 60, context, "track_descriptor", required=False)
    if (event is None) == (descriptor is None):
        raise DecodeError(f"{context}.data: expected exactly one oneof member")
    if descriptor is not None:
        if timestamp is not None or clock is not None:
            raise DecodeError(f"{context}: descriptor packet must not carry a timestamp")
        return {
            "kind": "descriptor",
            "descriptor": _decode_descriptor(_bytes(descriptor), packet_index),
            "unknown_fields": unknown,
        }

    if timestamp is None or clock is None:
        raise DecodeError(f"{context}: event packet requires timestamp and clock id")
    if _integer(clock) != 11:
        raise DecodeError(f"{context}.timestamp_clock_id: expected 11")
    assert event is not None
    return {
        "kind": "event",
        "timestamp": str(_integer(timestamp)),
        "clock_id": "11",
        "event": _decode_track_event(_bytes(event), packet_index),
        "unknown_fields": unknown,
    }


def decode_trace(data: bytes) -> dict[str, Any]:
    fields, unknown = _partition(data, "Trace", {1: ("packet", 2)})
    packets = [
        _decode_packet(_bytes(field), index) for index, field in enumerate(fields[1])
    ]
    if not packets:
        raise DecodeError("Trace.packet: trace has no packets")
    trace = {"packets": packets, "unknown_fields": unknown}
    validate_trace(trace)
    return trace


def _annotation_value(annotation: dict[str, Any], name: str) -> Any:
    for key in ("bool", "uint", "int", "double", "string"):
        if key in annotation:
            return annotation[key]
    raise DecodeError(f"annotation {name!r} has no decoded value")


def _metadata(name: str) -> dict[str, str]:
    prefix = "Troupe metadata | "
    if not name.startswith(prefix):
        raise DecodeError("metadata descriptor has an invalid prefix")
    fields: dict[str, str] = {}
    order = []
    for part in name[len(prefix) :].split(" | "):
        key, separator, value = part.partition("=")
        if not separator or not key or key in fields:
            raise DecodeError("metadata descriptor has malformed or duplicate fields")
        fields[key] = value
        order.append(key)
    expected = [
        "exporter_schema",
        "event_schema",
        "run_id",
        "captured_watermark",
        "exported_through",
        "troupe_version",
        "outcome",
        "clean_shutdown",
        "content_warning",
    ]
    if order != expected:
        raise DecodeError("metadata descriptor fields are missing, reordered, or unknown")
    if fields["exporter_schema"] != "1" or fields["event_schema"] != "1":
        raise DecodeError("metadata descriptor schema version mismatch")
    run_id = fields["run_id"]
    if re.fullmatch(
        r"[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}",
        run_id,
    ) is None:
        raise DecodeError("metadata descriptor run_id is not a canonical UUIDv4")
    for key in ("captured_watermark", "exported_through"):
        if re.fullmatch(r"0|[1-9][0-9]*", fields[key]) is None:
            raise DecodeError(f"metadata descriptor {key} is not canonical u64")
        if int(fields[key]) > (1 << 64) - 1:
            raise DecodeError(f"metadata descriptor {key} exceeds u64")
    if int(fields["exported_through"]) > int(fields["captured_watermark"]):
        raise DecodeError("metadata exported_through exceeds captured_watermark")
    if not fields["troupe_version"]:
        raise DecodeError("metadata troupe_version is empty")
    if fields["outcome"] not in {"completed", "failed", "cancelled", "unavailable"}:
        raise DecodeError("metadata outcome is invalid")
    if fields["clean_shutdown"] not in {"true", "false", "unavailable"}:
        raise DecodeError("metadata clean_shutdown is invalid")
    if fields["content_warning"] != CONTENT_WARNING:
        raise DecodeError("metadata content warning drift")
    return fields


def validate_trace(trace: dict[str, Any]) -> None:
    descriptors: dict[str, dict[str, Any]] = {}
    root: dict[str, Any] | None = None
    metadata_descriptor: dict[str, Any] | None = None
    saw_event = False
    previous_timestamp = 0
    sequences: set[int] = set()

    for packet in trace["packets"]:
        if packet["kind"] == "descriptor":
            if saw_event:
                raise DecodeError("descriptor packet appears after an event packet")
            descriptor = packet["descriptor"]
            uuid = descriptor["uuid"]
            if uuid in descriptors:
                raise DecodeError(f"duplicate descriptor uuid {uuid}")
            parent = descriptor["parent_uuid"]
            if parent is not None and parent not in descriptors:
                raise DecodeError(f"descriptor {uuid} appears before parent {parent}")
            descriptors[uuid] = descriptor
            if descriptor["name"].startswith("Troupe Production "):
                if root is not None or parent is not None:
                    raise DecodeError("trace must have one parentless Troupe Production root")
                root = descriptor
            if descriptor["name"].startswith("Troupe metadata"):
                if metadata_descriptor is not None:
                    raise DecodeError("trace has multiple metadata descriptors")
                metadata_descriptor = descriptor
            continue

        saw_event = True
        event = packet["event"]
        if event["track_uuid"] not in descriptors:
            raise DecodeError(f"event references unknown track {event['track_uuid']}")
        timestamp = int(packet["timestamp"])
        if timestamp < previous_timestamp:
            raise DecodeError("event timestamps regress")
        previous_timestamp = timestamp
        annotations = {item["name"]: item for item in event["annotations"]}
        if len(annotations) != len(event["annotations"]):
            raise DecodeError("event has duplicate annotation names")
        kind = annotations.get("troupe.event.kind")
        if kind is None or _annotation_value(kind, kind["name"]) != event["type"]:
            raise DecodeError("event kind annotation does not match TrackEvent type")
        sequence = annotations.get("troupe.event.sequence")
        if sequence is None or "uint" not in sequence:
            raise DecodeError("event has no uint troupe.event.sequence annotation")
        parsed_sequence = int(sequence["uint"])
        if parsed_sequence <= 0:
            raise DecodeError("event sequence must be positive")
        sequences.add(parsed_sequence)

    if root is None or metadata_descriptor is None:
        raise DecodeError("trace is missing root or metadata descriptor")
    if metadata_descriptor["parent_uuid"] != root["uuid"]:
        raise DecodeError("metadata descriptor is not a child of the root track")
    metadata = _metadata(metadata_descriptor["name"])
    if root["name"] != f"Troupe Production {metadata['run_id']}":
        raise DecodeError("root track Run identity disagrees with metadata")
    through = int(metadata["exported_through"])
    if sequences != set(range(1, through + 1)):
        raise DecodeError("event sequence annotations do not cover exact exported prefix")


def _unknown_count(trace: dict[str, Any]) -> int:
    count = len(trace["unknown_fields"])
    for packet in trace["packets"]:
        count += len(packet["unknown_fields"])
        if packet["kind"] == "descriptor":
            count += len(packet["descriptor"]["unknown_fields"])
        else:
            event = packet["event"]
            count += len(event["unknown_fields"])
            count += sum(len(item["unknown_fields"]) for item in event["annotations"])
    return count


def summarize_trace(trace: dict[str, Any]) -> dict[str, Any]:
    descriptors = [
        packet["descriptor"]
        for packet in trace["packets"]
        if packet["kind"] == "descriptor"
    ]
    events = [
        packet["event"] for packet in trace["packets"] if packet["kind"] == "event"
    ]
    metadata_name = next(
        descriptor["name"]
        for descriptor in descriptors
        if descriptor["name"].startswith("Troupe metadata | ")
    )
    metadata = _metadata(metadata_name)
    type_counts = Counter(event["type"] for event in events)
    counter_values = [
        {"name": event["name"], **event["counter"]}
        for event in events
        if event["counter"] is not None
    ]
    return {
        "packet_count": len(trace["packets"]),
        "descriptor_count": len(descriptors),
        "event_count": len(events),
        "track_names": [
            descriptor["name"]
            for descriptor in descriptors
            if not descriptor["name"].startswith("Troupe Production ")
            and not descriptor["name"].startswith("Troupe metadata | ")
        ],
        "event_types": dict(sorted(type_counts.items())),
        "event_names": [event["name"] for event in events],
        "counter_values": counter_values,
        "flow_ids": [value for event in events for value in event["flow_ids"]],
        "terminating_flow_ids": [
            value for event in events for value in event["terminating_flow_ids"]
        ],
        "annotation_names": sorted(
            {annotation["name"] for event in events for annotation in event["annotations"]}
        ),
        "metadata": {
            key: metadata[key]
            for key in (
                "run_id",
                "captured_watermark",
                "exported_through",
                "troupe_version",
                "outcome",
                "clean_shutdown",
            )
        },
        "unknown_field_count": _unknown_count(trace),
    }


def _json_object(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
    result: dict[str, Any] = {}
    for key, value in pairs:
        if key in result:
            raise CompatibilityError(f"duplicate JSON key {key!r}")
        result[key] = value
    return result


def _load_json(path: Path) -> dict[str, Any]:
    try:
        value = json.loads(path.read_text(encoding="utf-8"), object_pairs_hook=_json_object)
    except (OSError, UnicodeError, json.JSONDecodeError) as error:
        raise CompatibilityError(f"cannot read {path}: {error}") from error
    if not isinstance(value, dict):
        raise CompatibilityError(f"{path} must contain a JSON object")
    return value


def _exact_keys(value: dict[str, Any], expected: set[str], context: str) -> None:
    if set(value) != expected:
        raise CompatibilityError(
            f"{context} keys differ: expected {sorted(expected)}, got {sorted(value)}"
        )


def _safe_path(value: Any, context: str) -> str:
    if not isinstance(value, str) or not value:
        raise CompatibilityError(f"{context} must be a non-empty string")
    path = PurePosixPath(value)
    if path.is_absolute() or ".." in path.parts or str(path) != value:
        raise CompatibilityError(f"{context} is not a safe repository-relative path")
    return value


def _sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def _sha256_value(value: Any, context: str) -> str:
    if not isinstance(value, str) or re.fullmatch(r"[0-9a-f]{64}", value) is None:
        raise CompatibilityError(f"{context} must be a lowercase SHA-256 digest")
    return value


def verify_repository(root: Path) -> dict[str, dict[str, Any]]:
    decode_root = root / "tests/perfetto/decode"
    manifest = _load_json(decode_root / "fixtures.manifest.json")
    expectations = _load_json(decode_root / "expectations.json")
    trace_manifest = _load_json(root / "tests/fixtures/perfetto/traces/manifest.json")
    _exact_keys(manifest, {"schema", "files"}, "decode fixture manifest")
    _exact_keys(expectations, {"schema", "fixtures"}, "decode expectations")
    _exact_keys(trace_manifest, {"schema", "files"}, "T03 trace fixture manifest")
    if manifest["schema"] != "troupe.perfetto.decode-fixtures.v1":
        raise CompatibilityError("decode fixture manifest schema mismatch")
    if expectations["schema"] != "troupe.perfetto.decode-expectations.v1":
        raise CompatibilityError("decode expectations schema mismatch")
    if trace_manifest["schema"] != "troupe.perfetto.trace-fixtures.v1":
        raise CompatibilityError("T03 trace fixture manifest schema mismatch")
    if not isinstance(manifest["files"], list) or not isinstance(trace_manifest["files"], list):
        raise CompatibilityError("fixture manifest files must be arrays")
    if not isinstance(expectations["fixtures"], dict):
        raise CompatibilityError("decode expectations fixtures must be an object")

    source_entries: dict[str, dict[str, Any]] = {}
    source_order: list[str] = []
    for index, entry in enumerate(trace_manifest["files"]):
        if not isinstance(entry, dict):
            raise CompatibilityError(f"T03 trace entry {index} must be an object")
        _exact_keys(entry, {"path", "bytes", "sha256"}, f"T03 trace entry {index}")
        path = _safe_path(entry["path"], f"T03 trace entry {index}.path")
        if "/" in path or path in source_entries:
            raise CompatibilityError("T03 trace manifest path must be a unique basename")
        if (
            not isinstance(entry["bytes"], int)
            or isinstance(entry["bytes"], bool)
            or entry["bytes"] < 0
        ):
            raise CompatibilityError("T03 trace byte count must be a nonnegative integer")
        _sha256_value(entry["sha256"], f"T03 trace entry {index}.sha256")
        source_entries[path] = entry
        source_order.append(path)

    summaries: dict[str, dict[str, Any]] = {}
    seen_paths: set[str] = set()
    decode_order: list[str] = []
    for index, entry in enumerate(manifest["files"]):
        if not isinstance(entry, dict):
            raise CompatibilityError(f"decode fixture entry {index} must be an object")
        _exact_keys(entry, {"name", "path", "sha256"}, f"decode fixture entry {index}")
        name = entry["name"]
        if not isinstance(name, str) or name in summaries:
            raise CompatibilityError(f"decode fixture entry {index} has invalid/duplicate name")
        path = _safe_path(entry["path"], f"decode fixture entry {index}.path")
        _sha256_value(entry["sha256"], f"decode fixture entry {index}.sha256")
        if path in seen_paths:
            raise CompatibilityError(f"duplicate decode fixture path {path}")
        seen_paths.add(path)
        decode_order.append(f"{name}.pftrace")
        expected_path = f"tests/fixtures/perfetto/traces/{name}.pftrace"
        if path != expected_path:
            raise CompatibilityError(f"decode fixture {name} does not use its exact T03 path")
        source = source_entries.get(f"{name}.pftrace")
        if source is None or source["sha256"] != entry["sha256"]:
            raise CompatibilityError(f"decode fixture {name} drifts from the T03 manifest")
        try:
            data = (root / path).read_bytes()
        except OSError as error:
            raise CompatibilityError(f"cannot read decode fixture {path}: {error}") from error
        if len(data) != source["bytes"] or _sha256(data) != entry["sha256"]:
            raise CompatibilityError(f"decode fixture {name} byte/hash mismatch")
        try:
            summaries[name] = summarize_trace(decode_trace(data))
        except DecodeError as error:
            raise CompatibilityError(f"decode fixture {name} failed: {error}") from error

    if set(summaries) != TRACE_FIXTURES:
        raise CompatibilityError("decode manifest does not reference the exact nine T03 fixtures")
    if decode_order != source_order:
        raise CompatibilityError("decode manifest order differs from the T03 fixture manifest")
    if set(expectations["fixtures"]) != TRACE_FIXTURES:
        raise CompatibilityError("decode expectations do not cover the exact nine T03 fixtures")
    for name, summary in summaries.items():
        if summary != expectations["fixtures"][name]:
            raise CompatibilityError(f"decode expectation mismatch for {name}")
    if (
        root / "tests/fixtures/perfetto/traces/multi-cue.pftrace"
    ).read_bytes() != (
        root / "tests/fixtures/perfetto/traces/repeated-dump.pftrace"
    ).read_bytes():
        raise CompatibilityError("repeated dump fixture is not byte-identical")
    return summaries


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--offline", action="store_true")
    parser.add_argument(
        "--root",
        type=Path,
        default=Path(__file__).resolve().parents[3],
    )
    arguments = parser.parse_args(argv)
    if not arguments.offline:
        parser.error("--offline is required")
    try:
        summaries = verify_repository(arguments.root.resolve())
    except CompatibilityError as error:
        print(f"Perfetto decode compatibility failed: {error}", file=sys.stderr)
        return 1
    print(f"Perfetto decode compatibility: {len(summaries)} fixtures verified offline")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
