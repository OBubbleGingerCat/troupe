#!/usr/bin/env python3
"""Audit Troupe's closed, private Perfetto protobuf mirror without protoc."""

from __future__ import annotations

import argparse
import hashlib
import json
import re
import sys
from collections import Counter
from dataclasses import dataclass
from pathlib import Path, PurePosixPath
from typing import Any


PINNED_COMMIT = "da1d152cff27890903d158fe96751de3aab883cc"
PINNED_FILES = {
    "upstream/LICENSE": "9a682a56cffc9524dfa9b0b1c0dca9cb81a19e96d5bd0793aaf02c08a95ee7ca",
    "upstream/protos/perfetto/common/builtin_clock.proto": "ae1d1542bc0a1f1fbd29d9351f19e2a86bd8add28c8e06ef662a7beadb8226fe",
    "upstream/protos/perfetto/trace/trace.proto": "e9f9af3a0feb7ed1214aa0e075c50568c1fc4f825d1022b20e0f4df3e17cd264",
    "upstream/protos/perfetto/trace/trace_packet.proto": "5e0d9d8de9c5bf37d79051a6ca8c981565dfc2f4f9fb44557dae69f335f5c513",
    "upstream/protos/perfetto/trace/track_event/debug_annotation.proto": "3927ee166f7e7482465b5fe1cc43d82bd3c76ad03133532f13d1eb620d9f9cad",
    "upstream/protos/perfetto/trace/track_event/track_descriptor.proto": "3e46a7df6b7aa1efbd5a8f6cb57c4f81fd5cdde3ee5d2762a07e628c72bd01e6",
    "upstream/protos/perfetto/trace/track_event/track_event.proto": "0f1e23fd49dfcd7de85c58497567ca41ea6627560bdadd3c5d1267ea92c9e4e6",
}
SCALARS = {
    "bool",
    "bytes",
    "double",
    "fixed32",
    "fixed64",
    "float",
    "int32",
    "int64",
    "sfixed32",
    "sfixed64",
    "sint32",
    "sint64",
    "string",
    "uint32",
    "uint64",
}
TOP_KEYS = {"schema_version", "upstream_commit", "package", "root_definitions", "definitions"}
MESSAGE_KEYS = {"kind", "full_name", "file", "fields", "oneofs"}
ENUM_KEYS = {"kind", "full_name", "file", "values"}
FIELD_KEYS = {"name", "number", "type", "cardinality", "oneof", "packed"}
ONEOF_KEYS = {"name", "fields"}
VALUE_KEYS = {"name", "number"}


class AuditError(RuntimeError):
    pass


@dataclass(frozen=True)
class ProtoDefinition:
    kind: str
    full_name: str
    body: str


def fail(message: str) -> None:
    raise AuditError(message)


def object_without_duplicates(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
    result: dict[str, Any] = {}
    for key, value in pairs:
        if key in result:
            fail(f"duplicate JSON key: {key}")
        result[key] = value
    return result


def load_json(path: Path) -> dict[str, Any]:
    try:
        value = json.loads(path.read_text(encoding="utf-8"), object_pairs_hook=object_without_duplicates)
    except (OSError, UnicodeError, json.JSONDecodeError) as error:
        fail(f"cannot read {path}: {error}")
    if not isinstance(value, dict):
        fail(f"{path} must contain a JSON object")
    return value


def require_exact_keys(value: dict[str, Any], expected: set[str], where: str) -> None:
    if set(value) != expected:
        fail(f"{where} keys differ: expected {sorted(expected)}, got {sorted(value)}")


def safe_relative_path(raw: Any, where: str) -> str:
    if not isinstance(raw, str) or not raw:
        fail(f"{where} must be a non-empty string")
    path = PurePosixPath(raw)
    if path.is_absolute() or ".." in path.parts or str(path) != raw:
        fail(f"unsafe path at {where}: {raw!r}")
    return raw


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def parse_sums(path: Path) -> dict[str, str]:
    result: dict[str, str] = {}
    try:
        lines = path.read_text(encoding="ascii").splitlines()
    except (OSError, UnicodeError) as error:
        fail(f"cannot read {path}: {error}")
    for line_number, line in enumerate(lines, 1):
        match = re.fullmatch(r"([0-9a-f]{64})  (\S+)", line)
        if match is None:
            fail(f"invalid SHA256SUMS line {line_number}")
        relative = safe_relative_path(match.group(2), f"SHA256SUMS line {line_number}")
        if relative in result:
            fail(f"duplicate SHA256SUMS path: {relative}")
        result[relative] = match.group(1)
    return result


def strip_proto_comments(source: str) -> str:
    output: list[str] = []
    index = 0
    in_string = False
    escaped = False
    while index < len(source):
        char = source[index]
        next_char = source[index + 1] if index + 1 < len(source) else ""
        if in_string:
            output.append(char)
            if escaped:
                escaped = False
            elif char == "\\":
                escaped = True
            elif char == '"':
                in_string = False
            index += 1
        elif char == '"':
            in_string = True
            output.append(char)
            index += 1
        elif char == "/" and next_char == "/":
            index += 2
            while index < len(source) and source[index] not in "\r\n":
                index += 1
        elif char == "/" and next_char == "*":
            index += 2
            while index + 1 < len(source) and source[index : index + 2] != "*/":
                index += 1
            if index + 1 >= len(source):
                fail("unterminated block comment in proto")
            index += 2
        else:
            output.append(char)
            index += 1
    if in_string:
        fail("unterminated string in proto")
    return "".join(output)


def matching_brace(source: str, opening: int) -> int:
    depth = 0
    in_string = False
    escaped = False
    for index in range(opening, len(source)):
        char = source[index]
        if in_string:
            if escaped:
                escaped = False
            elif char == "\\":
                escaped = True
            elif char == '"':
                in_string = False
        elif char == '"':
            in_string = True
        elif char == "{":
            depth += 1
        elif char == "}":
            depth -= 1
            if depth == 0:
                return index
    fail("unbalanced proto braces")
    raise AssertionError


def collect_definitions(source: str, package: str) -> dict[str, ProtoDefinition]:
    definitions: dict[str, ProtoDefinition] = {}

    def walk(region: str, prefix: str) -> None:
        index = 0
        declaration = re.compile(r"(?:message|enum)\s+[A-Za-z_]\w*\s*\{")
        while index < len(region):
            match = declaration.search(region, index)
            if match is None:
                break
            # Only declarations at this region's top level belong to this scope.
            depth = 0
            for char in region[index : match.start()]:
                if char == "{":
                    depth += 1
                elif char == "}":
                    depth -= 1
            if depth != 0:
                index = match.end()
                continue
            header = region[match.start() : match.end()]
            header_match = re.match(r"(message|enum)\s+([A-Za-z_]\w*)", header)
            assert header_match is not None
            kind, name = header_match.groups()
            opening = match.end() - 1
            closing = matching_brace(region, opening)
            body = region[opening + 1 : closing]
            full_name = f"{prefix}.{name}"
            if full_name in definitions:
                fail(f"duplicate proto definition: {full_name}")
            definitions[full_name] = ProtoDefinition(kind=kind, full_name=full_name, body=body)
            if kind == "message":
                walk(body, full_name)
            index = closing + 1

    walk(strip_proto_comments(source), package)
    return definitions


def parse_field(statement: str, oneof: str | None = None) -> dict[str, Any] | None:
    normalized = " ".join(statement.split())
    match = re.fullmatch(
        r"(?:(optional|required|repeated)\s+)?([.]?[A-Za-z_]\w*(?:\.[A-Za-z_]\w*)*)\s+"
        r"([A-Za-z_]\w*)\s*=\s*(-?\d+)(?:\s*\[(.*)\])?",
        normalized,
    )
    if match is None:
        return None
    label, field_type, name, number, options = match.groups()
    cardinality = "oneof" if oneof is not None else (label or "optional")
    field: dict[str, Any] = {
        "name": name,
        "number": int(number),
        "raw_type": field_type.removeprefix("."),
        "cardinality": cardinality,
    }
    if oneof is not None:
        field["oneof"] = oneof
    if cardinality == "repeated" and field_type.removeprefix(".") in SCALARS:
        packed_match = re.search(r"(?:^|,)\s*packed\s*=\s*(true|false)(?:\s*,|$)", options or "")
        field["packed"] = packed_match is not None and packed_match.group(1) == "true"
    return field


def parse_message_fields(body: str) -> tuple[dict[str, dict[str, Any]], dict[str, list[str]]]:
    fields: dict[str, dict[str, Any]] = {}
    oneofs: dict[str, list[str]] = {}
    index = 0
    statement_start = 0
    while index < len(body):
        oneof_match = re.match(r"\s*oneof\s+([A-Za-z_]\w*)\s*\{", body[index:])
        definition_match = re.match(r"\s*(?:message|enum)\s+[A-Za-z_]\w*\s*\{", body[index:])
        if oneof_match is not None:
            prefix_length = oneof_match.end()
            opening = index + prefix_length - 1
            closing = matching_brace(body, opening)
            name = oneof_match.group(1)
            members: list[str] = []
            for statement in body[opening + 1 : closing].split(";"):
                field = parse_field(statement, name)
                if field is None:
                    continue
                if field["name"] in fields:
                    fail(f"duplicate proto field: {field['name']}")
                fields[field["name"]] = field
                members.append(field["name"])
            oneofs[name] = members
            index = closing + 1
            statement_start = index
        elif definition_match is not None:
            opening = index + definition_match.end() - 1
            index = matching_brace(body, opening) + 1
            statement_start = index
        elif body[index] == ";":
            field = parse_field(body[statement_start:index])
            if field is not None:
                if field["name"] in fields:
                    fail(f"duplicate proto field: {field['name']}")
                fields[field["name"]] = field
            index += 1
            statement_start = index
        elif body[index] == "{":
            index = matching_brace(body, index) + 1
            statement_start = index
        else:
            index += 1
    return fields, oneofs


def parse_enum_values(body: str) -> dict[str, int]:
    result: dict[str, int] = {}
    for statement in body.split(";"):
        normalized = " ".join(statement.split())
        match = re.fullmatch(r"([A-Za-z_]\w*)\s*=\s*(-?\d+)(?:\s*\[.*\])?", normalized)
        if match is not None:
            result[match.group(1)] = int(match.group(2))
    return result


def resolve_type(raw: str, containing: str, definitions: set[str]) -> str:
    if raw in SCALARS:
        return raw
    if raw in definitions:
        return raw
    scope = containing.split(".")
    while scope:
        candidate = ".".join([*scope, raw])
        if candidate in definitions:
            return candidate
        scope.pop()
    return raw


def validate_manifest_shape(manifest: dict[str, Any]) -> list[dict[str, Any]]:
    require_exact_keys(manifest, TOP_KEYS, "manifest")
    if manifest["schema_version"] != 1:
        fail("unsupported used-fields schema_version")
    if manifest["upstream_commit"] != PINNED_COMMIT:
        fail("used-fields upstream commit drift")
    if manifest["package"] != "perfetto.protos":
        fail("unexpected Perfetto package")
    if not isinstance(manifest["definitions"], list) or not manifest["definitions"]:
        fail("definitions must be a non-empty list")
    if not isinstance(manifest["root_definitions"], list) or not manifest["root_definitions"]:
        fail("root_definitions must be a non-empty list")

    names: set[str] = set()
    definitions: list[dict[str, Any]] = []
    for index, definition in enumerate(manifest["definitions"]):
        where = f"definitions[{index}]"
        if not isinstance(definition, dict):
            fail(f"{where} must be an object")
        kind = definition.get("kind")
        require_exact_keys(definition, MESSAGE_KEYS if kind == "message" else ENUM_KEYS, where)
        full_name = definition["full_name"]
        if not isinstance(full_name, str) or not full_name.startswith("perfetto.protos."):
            fail(f"invalid full_name at {where}")
        if full_name in names:
            fail(f"duplicate manifest definition: {full_name}")
        names.add(full_name)
        safe_relative_path(definition["file"], f"{where}.file")
        if kind == "message":
            if not isinstance(definition["fields"], list) or not isinstance(definition["oneofs"], list):
                fail(f"{where} fields/oneofs must be lists")
            field_names: set[str] = set()
            field_numbers: set[int] = set()
            for field_index, field in enumerate(definition["fields"]):
                field_where = f"{where}.fields[{field_index}]"
                if not isinstance(field, dict):
                    fail(f"{field_where} must be an object")
                allowed = set(FIELD_KEYS)
                required = {"name", "number", "type", "cardinality"}
                if not required <= set(field) or not set(field) <= allowed:
                    fail(f"{field_where} keys are not closed")
                if field["cardinality"] not in {"optional", "repeated", "oneof"}:
                    fail(f"invalid cardinality at {field_where}")
                if (field["cardinality"] == "oneof") != ("oneof" in field):
                    fail(f"oneof membership mismatch at {field_where}")
                if "packed" in field and (
                    field["cardinality"] != "repeated" or not isinstance(field["packed"], bool)
                ):
                    fail(f"invalid packed setting at {field_where}")
                if not isinstance(field["number"], int) or field["number"] <= 0:
                    fail(f"invalid field number at {field_where}")
                if field["name"] in field_names or field["number"] in field_numbers:
                    fail(f"duplicate field name/number at {field_where}")
                field_names.add(field["name"])
                field_numbers.add(field["number"])
            oneof_names: set[str] = set()
            listed_members: list[str] = []
            for oneof_index, oneof in enumerate(definition["oneofs"]):
                oneof_where = f"{where}.oneofs[{oneof_index}]"
                if not isinstance(oneof, dict):
                    fail(f"{oneof_where} must be an object")
                require_exact_keys(oneof, ONEOF_KEYS, oneof_where)
                if oneof["name"] in oneof_names or not isinstance(oneof["fields"], list):
                    fail(f"invalid oneof at {oneof_where}")
                oneof_names.add(oneof["name"])
                listed_members.extend(oneof["fields"])
                expected_members = [
                    field["name"] for field in definition["fields"] if field.get("oneof") == oneof["name"]
                ]
                if oneof["fields"] != expected_members:
                    fail(f"oneof field list drift at {oneof_where}")
            expected_listed = [field["name"] for field in definition["fields"] if "oneof" in field]
            if Counter(listed_members) != Counter(expected_listed):
                fail(f"oneof coverage drift at {where}")
        elif kind == "enum":
            if not isinstance(definition["values"], list) or not definition["values"]:
                fail(f"{where}.values must be a non-empty list")
            value_names: set[str] = set()
            value_numbers: set[int] = set()
            for value_index, value in enumerate(definition["values"]):
                value_where = f"{where}.values[{value_index}]"
                if not isinstance(value, dict):
                    fail(f"{value_where} must be an object")
                require_exact_keys(value, VALUE_KEYS, value_where)
                if value["name"] in value_names or value["number"] in value_numbers:
                    fail(f"duplicate enum name/number at {value_where}")
                value_names.add(value["name"])
                value_numbers.add(value["number"])
        else:
            fail(f"invalid definition kind at {where}")
        definitions.append(definition)

    roots = manifest["root_definitions"]
    if any(not isinstance(root, str) for root in roots) or len(set(roots)) != len(roots):
        fail("invalid root_definitions")
    if not set(roots) <= names:
        fail("root_definitions reference an unknown definition")
    return definitions


def audit_snapshots(schema_root: Path, manifest: dict[str, Any]) -> None:
    sums = parse_sums(schema_root / "SHA256SUMS")
    if sums != PINNED_FILES:
        fail("SHA256SUMS differs from the pinned Perfetto v57.2 file set")
    upstream_root = schema_root / "upstream"
    actual_files = {
        path.relative_to(schema_root).as_posix()
        for path in upstream_root.rglob("*")
        if path.is_file() and not path.is_symlink()
    }
    symlinks = [path for path in upstream_root.rglob("*") if path.is_symlink()]
    if symlinks:
        fail(f"upstream snapshot contains symlinks: {symlinks[0]}")
    if actual_files != set(PINNED_FILES):
        fail("upstream snapshot contains a missing or orphan file")
    for relative, expected in PINNED_FILES.items():
        path = schema_root / relative
        if sha256(path) != expected:
            fail(f"snapshot hash drift: {relative}")
    provenance = (schema_root / "PROVENANCE.md").read_text(encoding="utf-8")
    commits = set(re.findall(r"\b[0-9a-f]{40}\b", provenance))
    if commits != {PINNED_COMMIT}:
        fail("PROVENANCE.md does not name exactly the pinned commit")
    if manifest["upstream_commit"] != PINNED_COMMIT:
        fail("manifest provenance drift")


def audit_proto_closure(schema_root: Path, manifest: dict[str, Any], selected: list[dict[str, Any]]) -> None:
    parsed_by_file: dict[str, dict[str, ProtoDefinition]] = {}
    all_definitions: dict[str, ProtoDefinition] = {}
    for relative in sorted({definition["file"] for definition in selected}):
        if relative not in PINNED_FILES:
            fail(f"definition references an unpinned file: {relative}")
        source = (schema_root / relative).read_text(encoding="utf-8")
        parsed = collect_definitions(source, manifest["package"])
        parsed_by_file[relative] = parsed
        for name, definition in parsed.items():
            if name in all_definitions:
                fail(f"definition appears in multiple snapshots: {name}")
            all_definitions[name] = definition

    selected_names = {definition["full_name"] for definition in selected}
    dependencies: dict[str, set[str]] = {name: set() for name in selected_names}
    for definition in selected:
        name = definition["full_name"]
        actual = parsed_by_file[definition["file"]].get(name)
        if actual is None or actual.kind != definition["kind"]:
            fail(f"used definition missing or changed kind: {name}")
        if actual.kind == "enum":
            actual_values = parse_enum_values(actual.body)
            for value in definition["values"]:
                if actual_values.get(value["name"]) != value["number"]:
                    fail(f"enum value drift: {name}.{value['name']}")
            continue

        actual_fields, actual_oneofs = parse_message_fields(actual.body)
        for field in definition["fields"]:
            actual_field = actual_fields.get(field["name"])
            if actual_field is None:
                fail(f"used field missing: {name}.{field['name']}")
            resolved = resolve_type(actual_field["raw_type"], name, set(all_definitions))
            comparisons = {
                "number": actual_field["number"],
                "type": resolved,
                "cardinality": actual_field["cardinality"],
            }
            if "oneof" in field:
                comparisons["oneof"] = actual_field.get("oneof")
            if "packed" in field:
                comparisons["packed"] = actual_field.get("packed")
            for key, actual_value in comparisons.items():
                if field.get(key) != actual_value:
                    fail(f"field {key} drift: {name}.{field['name']}")
            if resolved not in SCALARS:
                if resolved not in selected_names:
                    fail(f"unclosed used type dependency: {name}.{field['name']} -> {resolved}")
                dependencies[name].add(resolved)
        for oneof in definition["oneofs"]:
            actual_selected = [member for member in actual_oneofs.get(oneof["name"], []) if member in actual_fields]
            if not set(oneof["fields"]) <= set(actual_selected):
                fail(f"oneof arm drift: {name}.{oneof['name']}")

    reached: set[str] = set()
    pending = list(manifest["root_definitions"])
    while pending:
        name = pending.pop()
        if name in reached:
            continue
        reached.add(name)
        pending.extend(sorted(dependencies[name] - reached))
    if reached != selected_names:
        fail(f"used-definition closure has unreachable definitions: {sorted(selected_names - reached)}")


def expected_marker_sets(selected: list[dict[str, Any]]) -> dict[str, set[str]]:
    expected = {kind: set() for kind in ("definition", "field", "oneof", "enum-value")}
    for definition in selected:
        name = definition["full_name"]
        expected["definition"].add(name)
        if definition["kind"] == "message":
            expected["field"].update(f"{name}.{field['name']}" for field in definition["fields"])
            expected["oneof"].update(f"{name}.{oneof['name']}" for oneof in definition["oneofs"])
        else:
            expected["enum-value"].update(f"{name}.{value['name']}" for value in definition["values"])
    return expected


def audit_rust_mirror(repo_root: Path, selected: list[dict[str, Any]]) -> None:
    source_path = repo_root / "rust/crates/troupe-diagnostics-perfetto/src/schema.rs"
    source = source_path.read_text(encoding="utf-8")
    lines = source.splitlines()
    marker_pattern = re.compile(r"\s*// perfetto-schema: (definition|field|oneof|enum-value) (\S+)\s*")
    actual: dict[str, set[str]] = {kind: set() for kind in ("definition", "field", "oneof", "enum-value")}
    marker_lines: dict[tuple[str, str], int] = {}
    for index, line in enumerate(lines):
        match = marker_pattern.fullmatch(line)
        if match is None:
            continue
        key = (match.group(1), match.group(2))
        if key in marker_lines:
            fail(f"duplicate Rust schema marker: {key[1]}")
        marker_lines[key] = index
        actual[key[0]].add(key[1])
    expected = expected_marker_sets(selected)
    if actual != expected:
        fail("Rust schema markers do not exactly match used-fields.json")

    definitions = {definition["full_name"]: definition for definition in selected}
    fields = {
        f"{definition['full_name']}.{field['name']}": field
        for definition in selected
        if definition["kind"] == "message"
        for field in definition["fields"]
    }
    for marker, field in fields.items():
        index = marker_lines[("field", marker)]
        if index + 1 >= len(lines):
            fail(f"missing prost attribute after marker: {marker}")
        attribute = lines[index + 1].strip()
        match = re.fullmatch(r"#\[prost\((.*)\)\]", attribute)
        if match is None:
            fail(f"field marker is not followed by a prost attribute: {marker}")
        arguments = match.group(1)
        tag_match = re.search(r"\btag\s*=\s*\"(\d+)\"", arguments)
        if tag_match is None or int(tag_match.group(1)) != field["number"]:
            fail(f"Rust tag drift: {marker}")
        field_type = field["type"]
        if field_type in SCALARS:
            wire_kind = field_type
        elif definitions[field_type]["kind"] == "enum":
            wire_kind = "enumeration"
        else:
            wire_kind = "message"
        if re.search(rf"(?:^|,\s*){re.escape(wire_kind)}(?:\s*=|\s*,|$)", arguments) is None:
            fail(f"Rust wire type drift: {marker}")
        if field["cardinality"] in {"optional", "repeated"} and not re.search(
            rf"(?:^|,\s*){field['cardinality']}(?:\s*,|$)", arguments
        ):
            fail(f"Rust cardinality drift: {marker}")
        if field["cardinality"] == "oneof" and re.search(r"\b(?:optional|repeated)\b", arguments):
            fail(f"Rust oneof cardinality drift: {marker}")
        if field.get("packed") is False and 'packed = "false"' not in arguments:
            fail(f"Rust packed encoding drift: {marker}")

    values = {
        f"{definition['full_name']}.{value['name']}": value["number"]
        for definition in selected
        if definition["kind"] == "enum"
        for value in definition["values"]
    }
    for marker, number in values.items():
        index = marker_lines[("enum-value", marker)]
        if index + 1 >= len(lines) or re.search(rf"=\s*{number}\s*,\s*$", lines[index + 1]) is None:
            fail(f"Rust enum value drift: {marker}")

    expected_oneofs = Counter()
    for definition in selected:
        if definition["kind"] != "message":
            continue
        field_by_name = {field["name"]: field for field in definition["fields"]}
        for oneof in definition["oneofs"]:
            tags = tuple(sorted(field_by_name[name]["number"] for name in oneof["fields"]))
            expected_oneofs[(oneof["name"], tags)] += 1
    actual_oneofs = Counter()
    wrapper_pattern = re.compile(
        r"#\[prost\(oneof\s*=\s*\"[^\"]+\",\s*tags\s*=\s*\"([^\"]+)\"\)\]\s*\n"
        r"\s*pub\(crate\)\s+([A-Za-z_]\w*)\s*:",
        re.MULTILINE,
    )
    for match in wrapper_pattern.finditer(source):
        tags = tuple(sorted(int(value.strip()) for value in match.group(1).split(",")))
        actual_oneofs[(match.group(2), tags)] += 1
    if actual_oneofs != expected_oneofs:
        fail("Rust oneof wrapper/tag closure drift")

    direct_attribute_lines = {
        index
        for index, line in enumerate(lines)
        if re.fullmatch(r"\s*#\[prost\((?!oneof\s*=).+\)\]\s*", line)
    }
    marked_attribute_lines = {index + 1 for (kind, _), index in marker_lines.items() if kind == "field"}
    if direct_attribute_lines != marked_attribute_lines:
        fail("Rust mirror contains an unmanifested prost field")
    if re.search(r"\bpub\s+(?:struct|enum)\b", source):
        fail("Perfetto wire mirror must remain crate-private")

    cargo = (repo_root / "rust/crates/troupe-diagnostics-perfetto/Cargo.toml").read_text(encoding="utf-8")
    prost_lines = [line.strip() for line in cargo.splitlines() if re.match(r"\s*prost\s*=", line)]
    if prost_lines != ['prost = { version = "=0.14.4" }']:
        fail("Perfetto crate must directly pin prost exactly to 0.14.4")
    forbidden = ("prost-build", "prost_build", "prost-types", "prost_types", "protoc")
    if any(token in cargo or token in source for token in forbidden):
        fail("Perfetto schema path contains a forbidden build/runtime schema tool")
    if (repo_root / "rust/crates/troupe-diagnostics-perfetto/build.rs").exists():
        fail("Perfetto schema crate must not have a build script")


def audit(repo_root: Path) -> None:
    schema_root = repo_root / "rust/crates/troupe-diagnostics-perfetto/schema"
    manifest = load_json(schema_root / "used-fields.json")
    selected = validate_manifest_shape(manifest)
    audit_snapshots(schema_root, manifest)
    audit_proto_closure(schema_root, manifest, selected)
    audit_rust_mirror(repo_root, selected)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--offline", action="store_true", help="document that no network access is permitted")
    parser.add_argument("--root", type=Path, help=argparse.SUPPRESS)
    arguments = parser.parse_args()
    repo_root = arguments.root.resolve() if arguments.root else Path(__file__).resolve().parents[1]
    try:
        audit(repo_root)
    except (AuditError, OSError, UnicodeError) as error:
        print(f"perfetto schema audit failed: {error}", file=sys.stderr)
        return 1
    print(f"Perfetto schema audit passed for {PINNED_COMMIT} ({len(PINNED_FILES)} files)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
