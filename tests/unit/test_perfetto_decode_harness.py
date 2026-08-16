from __future__ import annotations

import importlib.util
import shutil
import struct
import subprocess
import sys
from pathlib import Path

import pytest


REPO_ROOT = Path(__file__).resolve().parents[2]
DECODER_PATH = REPO_ROOT / "tests/perfetto/decode/decoder.py"
EMPTY_TRACE = REPO_ROOT / "tests/fixtures/perfetto/traces/empty.pftrace"
NUMERIC_TRACE = REPO_ROOT / "tests/fixtures/perfetto/traces/numeric-boundary.pftrace"


def _load_decoder():
    specification = importlib.util.spec_from_file_location("troupe_perfetto_decoder", DECODER_PATH)
    assert specification is not None and specification.loader is not None
    module = importlib.util.module_from_spec(specification)
    sys.modules[specification.name] = module
    specification.loader.exec_module(module)
    return module


decoder = _load_decoder()


def _varint(value: int) -> bytes:
    output = bytearray()
    while value >= 0x80:
        output.append((value & 0x7F) | 0x80)
        value >>= 7
    output.append(value)
    return bytes(output)


def _key(number: int, wire_type: int) -> bytes:
    return _varint((number << 3) | wire_type)


def _uint(number: int, value: int) -> bytes:
    return _key(number, 0) + _varint(value)


def _fixed64(number: int, value: int) -> bytes:
    return _key(number, 1) + struct.pack("<Q", value)


def _message(number: int, value: bytes) -> bytes:
    return _key(number, 2) + _varint(len(value)) + value


def _text(number: int, value: str) -> bytes:
    return _message(number, value.encode("utf-8"))


def _synthetic_flow_trace() -> bytes:
    run_id = "12345678-1234-4234-9234-123456789abc"
    root = _uint(1, 1) + _text(2, f"Troupe Production {run_id}")
    metadata_name = (
        "Troupe metadata | exporter_schema=1 | event_schema=1 | "
        f"run_id={run_id} | captured_watermark=1 | exported_through=1 | "
        "troupe_version=test | outcome=unavailable | clean_shutdown=unavailable | "
        "content_warning=trace may contain sensitive diagnostic metadata and user-provided attributes"
    )
    metadata = _uint(1, 2) + _text(2, metadata_name) + _uint(5, 1)
    annotation_kind = _text(10, "troupe.event.kind") + _text(6, "instant")
    annotation_sequence = _text(10, "troupe.event.sequence") + _uint(3, 1)
    event = (
        _message(4, annotation_kind)
        + _message(4, annotation_sequence)
        + _uint(9, 3)
        + _uint(11, 1)
        + _text(23, "flow.test")
        + _fixed64(47, 7)
        + _fixed64(48, 8)
    )
    descriptor_packet = _message(60, root)
    metadata_packet = _message(60, metadata)
    event_packet = _uint(8, 5) + _message(11, event) + _uint(58, 11)
    return _message(1, descriptor_packet) + _message(1, metadata_packet) + _message(1, event_packet)


def test_offline_compatibility_script_accepts_all_checked_fixtures() -> None:
    result = subprocess.run(
        [str(REPO_ROOT / "scripts/test_perfetto_decode_compatibility.sh"), "--offline"],
        cwd=REPO_ROOT,
        check=False,
        capture_output=True,
        text=True,
    )

    assert result.returncode == 0, result.stderr
    assert "9 fixtures verified offline" in result.stdout


def test_known_field_with_wrong_wire_type_is_rejected() -> None:
    data = bytearray(EMPTY_TRACE.read_bytes())
    assert data[0] == 0x0A
    data[0] = 0x08

    with pytest.raises(decoder.DecodeError, match=r"Trace\.packet: wire type 0, expected 2"):
        decoder.decode_trace(bytes(data))


def test_truncated_trace_is_rejected() -> None:
    with pytest.raises(decoder.DecodeError, match="truncated"):
        decoder.decode_trace(EMPTY_TRACE.read_bytes()[:-1])


def test_mutated_checked_fixture_fails_hash_binding(tmp_path: Path) -> None:
    root = tmp_path / "repo"
    shutil.copytree(REPO_ROOT / "tests/perfetto/decode", root / "tests/perfetto/decode")
    shutil.copytree(
        REPO_ROOT / "tests/fixtures/perfetto/traces",
        root / "tests/fixtures/perfetto/traces",
    )
    trace = root / "tests/fixtures/perfetto/traces/open.pftrace"
    data = bytearray(trace.read_bytes())
    data[-1] ^= 1
    trace.write_bytes(data)

    with pytest.raises(decoder.CompatibilityError, match="byte/hash mismatch"):
        decoder.verify_repository(root)


def test_unknown_field_is_preserved_without_becoming_a_known_packet() -> None:
    original = decoder.decode_trace(EMPTY_TRACE.read_bytes())
    extended = decoder.decode_trace(EMPTY_TRACE.read_bytes() + _uint(2, 1))

    assert extended["packets"] == original["packets"]
    assert extended["unknown_fields"] == [{"number": 2, "wire_type": 0}]
    assert decoder.summarize_trace(extended)["unknown_field_count"] == 1


def test_synthetic_packet_decodes_unpacked_flow_and_annotations() -> None:
    summary = decoder.summarize_trace(decoder.decode_trace(_synthetic_flow_trace()))

    assert summary["event_types"] == {"instant": 1}
    assert summary["flow_ids"] == ["7"]
    assert summary["terminating_flow_ids"] == ["8"]
    assert summary["annotation_names"] == [
        "troupe.event.kind",
        "troupe.event.sequence",
    ]


def test_numeric_fixture_preserves_int64_boundary_and_decimal_fallback() -> None:
    summary = decoder.summarize_trace(decoder.decode_trace(NUMERIC_TRACE.read_bytes()))

    assert summary["counter_values"] == [
        {"name": "numeric.i64_max", "int64": "9223372036854775807"}
    ]
    assert "troupe.counter.value_decimal" in summary["annotation_names"]
    assert "troupe.counter_projection" in summary["annotation_names"]


def test_active_and_archive_metadata_keep_capture_and_completion_distinct() -> None:
    active = decoder.summarize_trace(
        decoder.decode_trace(
            (REPO_ROOT / "tests/fixtures/perfetto/traces/active-watermark.pftrace").read_bytes()
        )
    )["metadata"]
    archive = decoder.summarize_trace(
        decoder.decode_trace(
            (REPO_ROOT / "tests/fixtures/perfetto/traces/archive-watermark.pftrace").read_bytes()
        )
    )["metadata"]

    assert active["captured_watermark"] == archive["captured_watermark"] == "3"
    assert active["exported_through"] == archive["exported_through"] == "2"
    assert (active["outcome"], active["clean_shutdown"]) == (
        "unavailable",
        "unavailable",
    )
    assert (archive["outcome"], archive["clean_shutdown"]) == ("completed", "true")


def test_repeated_dump_is_byte_identical() -> None:
    traces = REPO_ROOT / "tests/fixtures/perfetto/traces"
    assert (traces / "multi-cue.pftrace").read_bytes() == (
        traces / "repeated-dump.pftrace"
    ).read_bytes()
