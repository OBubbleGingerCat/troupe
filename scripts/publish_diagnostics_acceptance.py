#!/usr/bin/env python3
from __future__ import annotations

import argparse
import errno
import hashlib
import json
import os
import re
import secrets
import stat
import sys
import uuid
from pathlib import Path
from typing import Any, Mapping, NoReturn, Sequence


ROOT = Path(__file__).resolve().parents[1]
PERFORMANCE_SCHEMA = (
    ROOT / "frontend/diagnostics/tests/stress/performance-raw.schema.json"
)
WHEEL_SCHEMA = ROOT / "tests/fixtures/release/diagnostics-wheel-report-schema.json"
FINAL_SCHEMA = ROOT / "tests/fixtures/release/diagnostics-final-evidence-schema.json"
ACCEPTED_SCHEMA = (
    ROOT / "tests/fixtures/release/diagnostics-accepted-evidence-schema.json"
)
REPORT_NAMES = {
    "performance": "V05-performance-raw.json",
    "wheel": "V07-wheel-report.json",
    "final": "V03-final-evidence.json",
}
FINAL_CHILD_NAMES = (
    "linux-release",
    "diagnostics-e2e",
    "O00",
    "O01",
    "O02",
    "O03",
    "O04",
    "V11",
    "plan",
    "ownership",
    "generated-diff",
)
IDENTITY_PATHS = {
    "actor_design_sha256": ROOT / "docs/design/actor-agent-session.md",
    "diagnostics_design_sha256": ROOT / "docs/design/production-diagnostics.md",
    "plan_sha256": ROOT / "docs/plan/production-diagnostics-implementation-plan.md",
    "validator_sha256": ROOT / "docs/plan/verify_production_diagnostics_plan.py",
    "review_record_sha256": ROOT
    / "docs/plan/production-diagnostics-plan-review-record.md",
}
SHA256_RE = re.compile(r"^[0-9a-f]{64}$")
COMMIT_RE = re.compile(r"^[0-9a-f]{40}$")


class AcceptanceError(RuntimeError):
    pass


class PublicationIndeterminate(AcceptanceError):
    pass


class PublisherIO:
    def open_directory(self, path: Path) -> int:
        flags = (
            os.O_RDONLY | getattr(os, "O_DIRECTORY", 0) | getattr(os, "O_NOFOLLOW", 0)
        )
        return os.open(path, flags)

    def stat_at(self, directory_fd: int, name: str) -> os.stat_result:
        return os.stat(name, dir_fd=directory_fd, follow_symlinks=False)

    def open_staging(self, directory_fd: int, name: str) -> int:
        flags = os.O_RDWR | os.O_CREAT | os.O_EXCL | getattr(os, "O_NOFOLLOW", 0)
        return os.open(name, flags, 0o600, dir_fd=directory_fd)

    def write(self, descriptor: int, payload: bytes) -> int:
        return os.write(descriptor, payload)

    def fsync_file(self, descriptor: int) -> None:
        os.fsync(descriptor)

    def link(self, directory_fd: int, source: str, target: str) -> None:
        os.link(
            source,
            target,
            src_dir_fd=directory_fd,
            dst_dir_fd=directory_fd,
            follow_symlinks=False,
        )

    def unlink(self, directory_fd: int, name: str) -> None:
        os.unlink(name, dir_fd=directory_fd)

    def fsync_directory(self, descriptor: int) -> None:
        os.fsync(descriptor)

    def fstat(self, descriptor: int) -> os.stat_result:
        return os.fstat(descriptor)

    def pread(self, descriptor: int, size: int, offset: int) -> bytes:
        return os.pread(descriptor, size, offset)

    def close(self, descriptor: int) -> None:
        os.close(descriptor)


def _canonical_json(value: object) -> bytes:
    return json.dumps(value, sort_keys=True, separators=(",", ":")).encode("utf-8")


def _sha256_bytes(value: bytes) -> str:
    return hashlib.sha256(value).hexdigest()


def _sha256_path(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        while chunk := stream.read(1024 * 1024):
            digest.update(chunk)
    return digest.hexdigest()


def _pairs_object(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
    result: dict[str, Any] = {}
    for key, value in pairs:
        if key in result:
            raise AcceptanceError(f"duplicate JSON field: {key}")
        result[key] = value
    return result


def _load_json(path: Path, label: str) -> tuple[dict[str, Any], bytes]:
    try:
        metadata = path.lstat()
        resolved = path.resolve(strict=True)
    except OSError as error:
        raise AcceptanceError(f"{label} is unavailable: {error}") from error
    if not stat.S_ISREG(metadata.st_mode) or stat.S_ISLNK(metadata.st_mode):
        raise AcceptanceError(f"{label} must be a regular non-symlink file")
    if resolved != path:
        raise AcceptanceError(f"{label} must not use symlink indirection")
    if metadata.st_size > 256 * 1024 * 1024:
        raise AcceptanceError(f"{label} is too large")
    try:
        payload = path.read_bytes()
        value = json.loads(payload, object_pairs_hook=_pairs_object)
    except (OSError, UnicodeError, json.JSONDecodeError) as error:
        raise AcceptanceError(f"{label} is not valid JSON: {error}") from error
    if not isinstance(value, dict):
        raise AcceptanceError(f"{label} must contain a JSON object")
    return value, payload


def _schema_target(schema: Mapping[str, Any], reference: str) -> Mapping[str, Any]:
    if not reference.startswith("#/"):
        raise AcceptanceError(f"unsupported schema reference: {reference}")
    value: Any = schema
    for raw in reference[2:].split("/"):
        key = raw.replace("~1", "/").replace("~0", "~")
        if not isinstance(value, dict) or key not in value:
            raise AcceptanceError(f"unresolved schema reference: {reference}")
        value = value[key]
    if not isinstance(value, dict):
        raise AcceptanceError(f"schema reference is not an object: {reference}")
    return value


def _validate_schema_value(
    value: Any,
    rule: Mapping[str, Any],
    schema: Mapping[str, Any],
    location: str,
) -> None:
    reference = rule.get("$ref")
    if reference is not None:
        if not isinstance(reference, str):
            raise AcceptanceError("schema $ref must be a string")
        _validate_schema_value(
            value, _schema_target(schema, reference), schema, location
        )
    if "const" in rule and value != rule["const"]:
        raise AcceptanceError(f"{location} differs from its schema constant")
    if "enum" in rule and value not in rule["enum"]:
        raise AcceptanceError(f"{location} is outside its schema enum")

    expected_type = rule.get("type")
    matches = {
        "object": isinstance(value, dict),
        "array": isinstance(value, list),
        "string": isinstance(value, str),
        "integer": isinstance(value, int) and not isinstance(value, bool),
        "number": isinstance(value, (int, float)) and not isinstance(value, bool),
        "boolean": isinstance(value, bool),
        "null": value is None,
    }
    if expected_type is not None:
        types = [expected_type] if isinstance(expected_type, str) else expected_type
        if not isinstance(types, list) or not all(
            isinstance(item, str) for item in types
        ):
            raise AcceptanceError("schema type declaration is invalid")
        if not any(matches.get(item, False) for item in types):
            raise AcceptanceError(f"{location} has the wrong type")

    if isinstance(value, dict):
        required = rule.get("required", [])
        properties = rule.get("properties", {})
        if not isinstance(required, list) or not isinstance(properties, dict):
            raise AcceptanceError("schema object declaration is invalid")
        missing = set(required) - set(value)
        if missing:
            raise AcceptanceError(f"{location} is missing {sorted(missing)}")
        if rule.get("additionalProperties") is False:
            extra = set(value) - set(properties)
            if extra:
                raise AcceptanceError(f"{location} has extra fields {sorted(extra)}")
        for key, child in value.items():
            child_rule = properties.get(key)
            if child_rule is not None:
                if not isinstance(child_rule, dict):
                    raise AcceptanceError("schema property declaration is invalid")
                _validate_schema_value(child, child_rule, schema, f"{location}.{key}")

    if isinstance(value, list):
        minimum = rule.get("minItems")
        maximum = rule.get("maxItems")
        if minimum is not None and len(value) < minimum:
            raise AcceptanceError(f"{location} has too few items")
        if maximum is not None and len(value) > maximum:
            raise AcceptanceError(f"{location} has too many items")
        prefix = rule.get("prefixItems", [])
        if not isinstance(prefix, list):
            raise AcceptanceError("schema prefixItems declaration is invalid")
        for index, child_rule in enumerate(prefix):
            if index >= len(value):
                break
            if not isinstance(child_rule, dict):
                raise AcceptanceError("schema prefix item is invalid")
            _validate_schema_value(
                value[index], child_rule, schema, f"{location}[{index}]"
            )
        items = rule.get("items")
        if items is False and len(value) > len(prefix):
            raise AcceptanceError(f"{location} has disallowed trailing items")
        if isinstance(items, dict):
            start = len(prefix) if prefix else 0
            for index in range(start, len(value)):
                _validate_schema_value(
                    value[index], items, schema, f"{location}[{index}]"
                )

    if isinstance(value, str):
        minimum_length = rule.get("minLength")
        maximum_length = rule.get("maxLength")
        if minimum_length is not None and len(value) < minimum_length:
            raise AcceptanceError(f"{location} is too short")
        if maximum_length is not None and len(value) > maximum_length:
            raise AcceptanceError(f"{location} is too long")
        pattern = rule.get("pattern")
        if pattern is not None and re.search(pattern, value) is None:
            raise AcceptanceError(f"{location} does not match its schema pattern")

    if isinstance(value, (int, float)) and not isinstance(value, bool):
        minimum_value = rule.get("minimum")
        maximum_value = rule.get("maximum")
        if minimum_value is not None and value < minimum_value:
            raise AcceptanceError(f"{location} is below its schema minimum")
        if maximum_value is not None and value > maximum_value:
            raise AcceptanceError(f"{location} is above its schema maximum")


def validate_schema(
    value: Mapping[str, Any], schema: Mapping[str, Any], label: str
) -> None:
    _validate_schema_value(value, schema, schema, label)


def _exact_directory(path: Path, label: str) -> Path:
    if not path.is_absolute() or str(path) != os.path.abspath(path):
        raise AcceptanceError(f"{label} must be an absolute normalized path")
    try:
        metadata = path.lstat()
        resolved = path.resolve(strict=True)
    except OSError as error:
        raise AcceptanceError(f"{label} is unavailable: {error}") from error
    if not stat.S_ISDIR(metadata.st_mode) or stat.S_ISLNK(metadata.st_mode):
        raise AcceptanceError(f"{label} must be a real directory")
    if resolved != path:
        raise AcceptanceError(f"{label} must not use symlink indirection")
    return path


def _canonical_attempt_id(value: str) -> str:
    try:
        parsed = uuid.UUID(value)
    except ValueError as error:
        raise AcceptanceError("attempt ID must be a canonical UUIDv4") from error
    if parsed.version != 4 or str(parsed) != value:
        raise AcceptanceError("attempt ID must be a canonical UUIDv4")
    return value


def _result_sha(report: Mapping[str, Any]) -> str:
    material = dict(report)
    material.pop("result", None)
    return _sha256_bytes(_canonical_json(material))


def _performance_result_sha(report: Mapping[str, Any]) -> str:
    result = report["result"]
    assert isinstance(result, dict)
    material = {
        "samples": report["samples"],
        "summary": report["summary"],
        "status": result["status"],
        "violations": result["violations"],
    }
    return _sha256_bytes(_canonical_json(material))


def _child_result_sha(child: Mapping[str, Any]) -> str:
    return _sha256_bytes(
        _canonical_json(
            {
                "argv": child["argv"],
                "exit_code": child["exit_code"],
                "stdout_sha256": child["stdout_sha256"],
                "stderr_sha256": child["stderr_sha256"],
            }
        )
    )


def _identity(integration_sha: str) -> dict[str, str]:
    if COMMIT_RE.fullmatch(integration_sha) is None:
        raise AcceptanceError(
            "integration SHA must be 40 lowercase hexadecimal characters"
        )
    return {
        **{name: _sha256_path(path) for name, path in IDENTITY_PATHS.items()},
        "integration_sha": integration_sha,
    }


def _expect(condition: bool, message: str) -> None:
    if not condition:
        raise AcceptanceError(message)


def _read_and_validate_reports(
    attempt: Path,
    attempt_id: str,
    integration_sha: str,
) -> tuple[dict[str, dict[str, Any]], dict[str, bytes]]:
    schemas: dict[str, dict[str, Any]] = {}
    for name, path in {
        "performance": PERFORMANCE_SCHEMA,
        "wheel": WHEEL_SCHEMA,
        "final": FINAL_SCHEMA,
    }.items():
        schemas[name], _ = _load_json(path, f"{name} report schema")

    reports: dict[str, dict[str, Any]] = {}
    payloads: dict[str, bytes] = {}
    for name, filename in REPORT_NAMES.items():
        path = attempt / filename
        reports[name], payloads[name] = _load_json(path, f"{name} report")
        validate_schema(reports[name], schemas[name], f"{name} report")

    performance = reports["performance"]
    wheel = reports["wheel"]
    final = reports["final"]
    expected_identity = _identity(integration_sha)
    for name, report in reports.items():
        _expect(
            report["identity"] == expected_identity, f"{name} report identity mismatch"
        )

    _expect(performance["kind"] == "gate", "performance report must be a gate report")
    _expect(
        performance["result"]["status"] == "passed"
        and performance["result"]["violations"] == [],
        "performance report did not pass",
    )
    _expect(
        performance["result"]["result_sha256"] == _performance_result_sha(performance),
        "performance report result hash mismatch",
    )
    _expect(wheel["result"]["status"] == "passed", "wheel report did not pass")
    _expect(
        wheel["result"]["result_sha256"] == _result_sha(wheel),
        "wheel report result hash mismatch",
    )
    _expect(final["attempt_id"] == attempt_id, "final report attempt mismatch")
    _expect(final["result"]["status"] == "passed", "final report did not pass")
    _expect(
        final["result"]["result_sha256"] == _result_sha(final),
        "final report result hash mismatch",
    )
    for index, child in enumerate(final["children"], start=1):
        _expect(
            (child["index"], child["name"]) == (index, FINAL_CHILD_NAMES[index - 1]),
            f"final child order mismatch at index {index}",
        )
        _expect(
            child["result_sha256"] == _child_result_sha(child),
            f"final child result hash mismatch: {child['name']}",
        )

    performance_hash = _sha256_bytes(payloads["performance"])
    wheel_hash = _sha256_bytes(payloads["wheel"])
    performance_cache_hash = _sha256_bytes(
        _canonical_json(performance["environment"]["cache"])
    )
    wheel_cache_hash = _sha256_bytes(_canonical_json(wheel["cache"]))
    expected_references = {
        "performance": {
            "path": REPORT_NAMES["performance"],
            "sha256": performance_hash,
            "result_sha256": performance["result"]["result_sha256"],
            "cache_sha256": performance_cache_hash,
        },
        "wheel": {
            "path": REPORT_NAMES["wheel"],
            "sha256": wheel_hash,
            "result_sha256": wheel["result"]["result_sha256"],
            "cache_sha256": wheel_cache_hash,
        },
    }
    _expect(final["reports"] == expected_references, "final report references mismatch")
    _expect(
        final["cache"]["performance_report_cache_sha256"] == performance_cache_hash,
        "performance cache binding mismatch",
    )
    _expect(
        final["cache"]["wheel_report_cache_sha256"] == wheel_cache_hash,
        "wheel cache binding mismatch",
    )
    cache_material = dict(final["cache"])
    aggregate = cache_material.pop("aggregate_sha256")
    _expect(
        aggregate == _sha256_bytes(_canonical_json(cache_material)),
        "final cache aggregate hash mismatch",
    )
    return reports, payloads


def _hash_open_file(io: PublisherIO, descriptor: int) -> str:
    size = io.fstat(descriptor).st_size
    digest = hashlib.sha256()
    offset = 0
    while offset < size:
        chunk = io.pread(descriptor, min(1024 * 1024, size - offset), offset)
        if not chunk:
            raise AcceptanceError("staging file became unreadable")
        digest.update(chunk)
        offset += len(chunk)
    return digest.hexdigest()


def _same_published_inode(
    io: PublisherIO,
    directory_fd: int,
    output_name: str,
    staging_fd: int,
    expected_sha256: str,
) -> bool:
    try:
        output_metadata = io.stat_at(directory_fd, output_name)
        staging_metadata = io.fstat(staging_fd)
        content_sha256 = _hash_open_file(io, staging_fd)
    except (OSError, AcceptanceError):
        return False
    return (
        stat.S_ISREG(output_metadata.st_mode)
        and output_metadata.st_dev == staging_metadata.st_dev
        and output_metadata.st_ino == staging_metadata.st_ino
        and content_sha256 == expected_sha256
    )


def _rollback_published(
    io: PublisherIO,
    directory_fd: int,
    output_name: str,
    staging_fd: int,
    expected_sha256: str,
    cause: BaseException,
) -> None:
    if not _same_published_inode(
        io, directory_fd, output_name, staging_fd, expected_sha256
    ):
        raise PublicationIndeterminate(
            f"published output identity changed after {cause}"
        ) from cause
    try:
        io.unlink(directory_fd, output_name)
    except OSError as error:
        raise PublicationIndeterminate(
            f"could not roll back published output after {cause}: {error}"
        ) from error
    try:
        io.fsync_directory(directory_fd)
    except OSError as error:
        raise PublicationIndeterminate(
            f"could not durably record publication rollback after {cause}: {error}"
        ) from error


def atomic_publish(
    output: Path,
    payload: bytes,
    *,
    io: PublisherIO | None = None,
) -> None:
    adapter = PublisherIO() if io is None else io
    directory_fd: int | None = None
    staging_fd: int | None = None
    staging_name: str | None = None
    preserve_staging = False
    expected_sha256 = _sha256_bytes(payload)
    try:
        try:
            directory_fd = adapter.open_directory(output.parent)
        except OSError as error:
            raise AcceptanceError(
                f"could not open acceptance directory: {error}"
            ) from error
        try:
            adapter.stat_at(directory_fd, output.name)
        except FileNotFoundError:
            pass
        except OSError as error:
            if error.errno != errno.ENOENT:
                raise AcceptanceError(
                    f"could not inspect acceptance output: {error}"
                ) from error
        else:
            raise AcceptanceError("acceptance output already exists")

        for _ in range(32):
            candidate = f".{output.name}.{os.getpid()}.{secrets.token_hex(8)}.tmp"
            try:
                staging_fd = adapter.open_staging(directory_fd, candidate)
            except FileExistsError:
                continue
            except OSError as error:
                raise AcceptanceError(
                    f"could not create acceptance staging file: {error}"
                ) from error
            staging_name = candidate
            break
        else:
            raise AcceptanceError("could not allocate an acceptance staging name")

        offset = 0
        try:
            while offset < len(payload):
                written = adapter.write(staging_fd, payload[offset:])
                if written <= 0:
                    raise OSError(errno.EIO, "zero-length staging write")
                offset += written
            adapter.fsync_file(staging_fd)
        except OSError as error:
            raise AcceptanceError(
                f"could not persist acceptance staging file: {error}"
            ) from error

        try:
            adapter.link(directory_fd, staging_name, output.name)
        except OSError as error:
            try:
                exists_as_staging = _same_published_inode(
                    adapter,
                    directory_fd,
                    output.name,
                    staging_fd,
                    expected_sha256,
                )
            except OSError:
                exists_as_staging = False
            if exists_as_staging:
                try:
                    _rollback_published(
                        adapter,
                        directory_fd,
                        output.name,
                        staging_fd,
                        expected_sha256,
                        error,
                    )
                except PublicationIndeterminate:
                    preserve_staging = True
                    raise
            raise AcceptanceError(
                f"could not publish acceptance without overwrite: {error}"
            ) from error

        try:
            adapter.unlink(directory_fd, staging_name)
            staging_name = None
            adapter.fsync_directory(directory_fd)
        except OSError as error:
            try:
                _rollback_published(
                    adapter,
                    directory_fd,
                    output.name,
                    staging_fd,
                    expected_sha256,
                    error,
                )
            except PublicationIndeterminate:
                preserve_staging = True
                raise
            raise AcceptanceError(
                f"acceptance publication rolled back after {error}"
            ) from error
    finally:
        if (
            directory_fd is not None
            and staging_name is not None
            and not preserve_staging
        ):
            try:
                adapter.unlink(directory_fd, staging_name)
            except OSError:
                pass
        if staging_fd is not None:
            try:
                adapter.close(staging_fd)
            except OSError:
                pass
        if directory_fd is not None:
            try:
                adapter.close(directory_fd)
            except OSError:
                pass


def publish_acceptance(
    evidence_base: Path,
    attempt_id: str,
    integration_sha: str,
    output: Path,
    *,
    io: PublisherIO | None = None,
) -> dict[str, Any]:
    base = _exact_directory(evidence_base, "evidence base")
    identifier = _canonical_attempt_id(attempt_id)
    attempts = _exact_directory(base / "attempts", "attempts directory")
    attempt = _exact_directory(attempts / identifier, "attempt directory")
    if attempt.parent != attempts:
        raise AcceptanceError("attempt directory must be directly below attempts")
    if output != base / "accepted.json":
        raise AcceptanceError(
            "output must be the exact accepted.json child of evidence base"
        )
    if output.exists() or output.is_symlink():
        raise AcceptanceError("acceptance output already exists")

    reports, payloads = _read_and_validate_reports(attempt, identifier, integration_sha)
    final = reports["final"]
    accepted_schema, _ = _load_json(ACCEPTED_SCHEMA, "accepted evidence schema")
    accepted: dict[str, Any] = {
        "schema": "troupe.diagnostics.accepted-evidence.v1",
        "attempt_id": identifier,
        "identity": final["identity"],
        "cache": final["cache"],
        "reports": {
            name: {
                "path": f"attempts/{identifier}/{REPORT_NAMES[name]}",
                "sha256": _sha256_bytes(payloads[name]),
                "result_sha256": reports[name]["result"]["result_sha256"],
            }
            for name in ("performance", "wheel", "final")
        },
    }
    accepted["result"] = {
        "status": "accepted",
        "result_sha256": _result_sha(accepted),
    }
    validate_schema(accepted, accepted_schema, "accepted evidence")
    atomic_publish(output, _canonical_json(accepted) + b"\n", io=io)
    return accepted


def _die(message: str, status: int) -> NoReturn:
    print(message, file=sys.stderr)
    raise SystemExit(status)


def main(arguments: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description="Publish immutable diagnostics acceptance evidence"
    )
    parser.add_argument("--evidence-base", required=True, type=Path)
    parser.add_argument("--attempt-id", required=True)
    parser.add_argument("--integration-sha", required=True)
    parser.add_argument("--output", required=True, type=Path)
    namespace = parser.parse_args(arguments)
    try:
        accepted = publish_acceptance(
            namespace.evidence_base,
            namespace.attempt_id,
            namespace.integration_sha,
            namespace.output,
        )
    except PublicationIndeterminate as error:
        _die(f"diagnostics acceptance publisher: publication_indeterminate: {error}", 3)
    except AcceptanceError as error:
        _die(f"diagnostics acceptance publisher: not_published: {error}", 1)
    print(
        json.dumps(
            {
                "status": "accepted",
                "output": str(namespace.output),
                "sha256": _sha256_path(namespace.output),
                "result_sha256": accepted["result"]["result_sha256"],
            },
            sort_keys=True,
            separators=(",", ":"),
        )
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
