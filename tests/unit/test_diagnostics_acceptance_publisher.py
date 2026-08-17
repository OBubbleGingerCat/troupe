from __future__ import annotations

import errno
import importlib.util
import json
import os
import sys
from pathlib import Path
from typing import Any, Mapping

import pytest


ROOT = Path(__file__).resolve().parents[2]
SCRIPT = ROOT / "scripts" / "publish_diagnostics_acceptance.py"
ATTEMPT_ID = "00000000-0000-4000-8000-000000000016"
INTEGRATION_SHA = "1" * 40

spec = importlib.util.spec_from_file_location(
    "diagnostics_acceptance_publisher", SCRIPT
)
assert spec is not None and spec.loader is not None
publisher = importlib.util.module_from_spec(spec)
sys.modules[spec.name] = publisher
spec.loader.exec_module(publisher)


def _schema_target(schema: Mapping[str, Any], reference: str) -> Mapping[str, Any]:
    value: Any = schema
    for key in reference[2:].split("/"):
        value = value[key]
    assert isinstance(value, dict)
    return value


def _minimal(rule: Mapping[str, Any], schema: Mapping[str, Any]) -> Any:
    if "$ref" in rule:
        return _minimal(_schema_target(schema, rule["$ref"]), schema)
    if "const" in rule:
        return rule["const"]
    if "enum" in rule:
        return rule["enum"][0]
    declared = rule.get("type")
    choices = [declared] if isinstance(declared, str) else declared or []
    kind = next((choice for choice in choices if choice != "null"), "null")
    if kind == "object":
        properties = rule.get("properties", {})
        return {
            name: _minimal(properties[name], schema)
            for name in rule.get("required", [])
        }
    if kind == "array":
        values = [_minimal(child, schema) for child in rule.get("prefixItems", [])]
        required = rule.get("minItems", 0)
        item = rule.get("items")
        while len(values) < required:
            assert isinstance(item, dict)
            values.append(_minimal(item, schema))
        return values
    if kind == "string":
        pattern = rule.get("pattern", "")
        if pattern == "^[0-9a-f]{64}$":
            return "a" * 64
        if pattern == "^[0-9a-f]{40}$":
            return "1" * 40
        if pattern == "^[0-9]+$":
            return "0"
        if "tar\\.gz" in pattern:
            return "troupe-0.1.0.tar.gz"
        if "\\.whl" in pattern:
            return "troupe-0.1.0-cp310-abi3-manylinux.whl"
        if "_runtime" in pattern:
            return "troupe/_runtime.abi3.so"
        if "4[0-9a-f]" in pattern:
            return ATTEMPT_ID
        return "x" * max(1, rule.get("minLength", 0))
    if kind in {"integer", "number"}:
        return rule.get("minimum", 0)
    if kind == "boolean":
        return False
    return None


def _load_schema(path: Path) -> dict[str, Any]:
    value = json.loads(path.read_text(encoding="utf-8"))
    assert isinstance(value, dict)
    return value


def _write_json(path: Path, value: Mapping[str, Any]) -> bytes:
    payload = publisher._canonical_json(value) + b"\n"
    path.write_bytes(payload)
    return payload


def _make_evidence(tmp_path: Path) -> tuple[Path, Path, dict[str, Any]]:
    base = (tmp_path / "evidence").resolve()
    attempt = base / "attempts" / ATTEMPT_ID
    attempt.mkdir(parents=True)
    identity = publisher._identity(INTEGRATION_SHA)

    performance_schema = _load_schema(publisher.PERFORMANCE_SCHEMA)
    performance = _minimal(performance_schema, performance_schema)
    performance["kind"] = "gate"
    performance["identity"] = identity
    performance["result"]["status"] = "passed"
    performance["result"]["violations"] = []
    performance["result"]["result_sha256"] = publisher._performance_result_sha(
        performance
    )
    publisher.validate_schema(performance, performance_schema, "test performance")
    performance_payload = _write_json(
        attempt / publisher.REPORT_NAMES["performance"], performance
    )

    wheel_schema = _load_schema(publisher.WHEEL_SCHEMA)
    wheel = _minimal(wheel_schema, wheel_schema)
    wheel["identity"] = identity
    wheel["result"]["status"] = "passed"
    wheel["result"]["result_sha256"] = publisher._result_sha(wheel)
    publisher.validate_schema(wheel, wheel_schema, "test wheel")
    wheel_payload = _write_json(attempt / publisher.REPORT_NAMES["wheel"], wheel)

    performance_cache_sha256 = publisher._sha256_bytes(
        publisher._canonical_json(performance["environment"]["cache"])
    )
    wheel_cache_sha256 = publisher._sha256_bytes(
        publisher._canonical_json(wheel["cache"])
    )
    cache = {
        "npm_manifest_sha256": "6" * 64,
        "playwright_identity_sha256": "7" * 64,
        "perfetto_identity_sha256": "8" * 64,
        "performance_report_cache_sha256": performance_cache_sha256,
        "wheel_report_cache_sha256": wheel_cache_sha256,
    }
    cache["aggregate_sha256"] = publisher._sha256_bytes(
        publisher._canonical_json(cache)
    )
    children = []
    for index, name in enumerate(publisher.FINAL_CHILD_NAMES, start=1):
        child = {
            "index": index,
            "name": name,
            "argv": [name],
            "exit_code": 0,
            "stdout_sha256": f"{index:x}" * 64,
            "stderr_sha256": f"{index + 1:x}" * 64,
        }
        child["stdout_sha256"] = child["stdout_sha256"][:64]
        child["stderr_sha256"] = child["stderr_sha256"][:64]
        child["result_sha256"] = publisher._child_result_sha(child)
        children.append(child)
    final: dict[str, Any] = {
        "schema": "troupe.diagnostics.final-evidence.v1",
        "attempt_id": ATTEMPT_ID,
        "identity": identity,
        "cache": cache,
        "children": children,
        "reports": {
            "performance": {
                "path": publisher.REPORT_NAMES["performance"],
                "sha256": publisher._sha256_bytes(performance_payload),
                "result_sha256": performance["result"]["result_sha256"],
                "cache_sha256": performance_cache_sha256,
            },
            "wheel": {
                "path": publisher.REPORT_NAMES["wheel"],
                "sha256": publisher._sha256_bytes(wheel_payload),
                "result_sha256": wheel["result"]["result_sha256"],
                "cache_sha256": wheel_cache_sha256,
            },
        },
    }
    final["result"] = {
        "status": "passed",
        "result_sha256": publisher._result_sha(final),
    }
    final_schema = _load_schema(publisher.FINAL_SCHEMA)
    publisher.validate_schema(final, final_schema, "test final")
    _write_json(attempt / publisher.REPORT_NAMES["final"], final)
    return base, attempt, final


class FaultIO(publisher.PublisherIO):
    def __init__(self, fault: str) -> None:
        self.fault = fault
        self.triggered: set[str] = set()
        self.open_descriptors: set[int] = set()
        self.directory_fsyncs = 0
        self.output_name = "accepted.json"

    def _once(self, name: str) -> bool:
        if self.fault != name or name in self.triggered:
            return False
        self.triggered.add(name)
        return True

    def open_directory(self, path: Path) -> int:
        if self._once("open-directory"):
            raise OSError(errno.EIO, "injected directory open")
        descriptor = super().open_directory(path)
        self.open_descriptors.add(descriptor)
        return descriptor

    def open_staging(self, directory_fd: int, name: str) -> int:
        if self._once("open-staging"):
            raise OSError(errno.EIO, "injected staging open")
        descriptor = super().open_staging(directory_fd, name)
        self.open_descriptors.add(descriptor)
        return descriptor

    def write(self, descriptor: int, payload: bytes) -> int:
        if self._once("write"):
            raise OSError(errno.ENOSPC, "injected write")
        return super().write(descriptor, payload)

    def fsync_file(self, descriptor: int) -> None:
        if self._once("file-fsync"):
            raise OSError(errno.EIO, "injected file fsync")
        super().fsync_file(descriptor)

    def link(self, directory_fd: int, source: str, target: str) -> None:
        self.output_name = target
        if self._once("link"):
            raise OSError(errno.EIO, "injected link")
        super().link(directory_fd, source, target)
        if self._once("link-post"):
            raise OSError(errno.EIO, "injected post-link result")

    def unlink(self, directory_fd: int, name: str) -> None:
        if name.startswith(".") and self._once("staging-unlink"):
            raise OSError(errno.EIO, "injected staging unlink")
        if name == self.output_name and self._once("rollback-unlink"):
            raise OSError(errno.EIO, "injected rollback unlink")
        super().unlink(directory_fd, name)

    def fsync_directory(self, descriptor: int) -> None:
        self.directory_fsyncs += 1
        if (
            self.fault in {"directory-fsync", "rollback-unlink"}
            and self.directory_fsyncs == 1
        ):
            raise OSError(errno.EIO, "injected directory fsync")
        if self.fault == "rollback-fsync" and self.directory_fsyncs <= 2:
            raise OSError(errno.EIO, "injected rollback fsync")
        if self.fault == "identity-mismatch" and self.directory_fsyncs == 1:
            os.unlink(self.output_name, dir_fd=descriptor)
            foreign = os.open(
                self.output_name,
                os.O_WRONLY | os.O_CREAT | os.O_EXCL,
                0o600,
                dir_fd=descriptor,
            )
            try:
                os.write(foreign, b"foreign\n")
                os.fsync(foreign)
            finally:
                os.close(foreign)
            raise OSError(errno.EIO, "injected identity race")
        super().fsync_directory(descriptor)

    def close(self, descriptor: int) -> None:
        try:
            super().close(descriptor)
        finally:
            self.open_descriptors.discard(descriptor)


def _staging_names(base: Path) -> list[str]:
    return [path.name for path in base.iterdir() if path.name.startswith(".")]


def test_success_publishes_one_closed_no_overwrite_record(tmp_path: Path) -> None:
    base, attempt, final = _make_evidence(tmp_path)
    output = base / "accepted.json"

    accepted = publisher.publish_acceptance(base, ATTEMPT_ID, INTEGRATION_SHA, output)

    assert json.loads(output.read_text(encoding="utf-8")) == accepted
    assert accepted["identity"] == final["identity"]
    assert accepted["cache"] == final["cache"]
    assert accepted["result"]["result_sha256"] == publisher._result_sha(accepted)
    assert output.stat().st_nlink == 1
    assert _staging_names(base) == []
    assert {path.name for path in attempt.iterdir()} == set(
        publisher.REPORT_NAMES.values()
    )
    accepted_schema = _load_schema(publisher.ACCEPTED_SCHEMA)
    publisher.validate_schema(accepted, accepted_schema, "published acceptance")

    with pytest.raises(publisher.AcceptanceError, match="already exists"):
        publisher.publish_acceptance(base, ATTEMPT_ID, INTEGRATION_SHA, output)
    assert json.loads(output.read_text(encoding="utf-8")) == accepted


@pytest.mark.parametrize(
    "fault",
    [
        "open-directory",
        "open-staging",
        "write",
        "file-fsync",
        "link",
        "link-post",
        "staging-unlink",
        "directory-fsync",
    ],
)
def test_io_faults_fail_closed_and_close_all_descriptors(
    tmp_path: Path, fault: str
) -> None:
    base, attempt, _ = _make_evidence(tmp_path)
    output = base / "accepted.json"
    adapter = FaultIO(fault)

    with pytest.raises(publisher.AcceptanceError) as raised:
        publisher.publish_acceptance(
            base, ATTEMPT_ID, INTEGRATION_SHA, output, io=adapter
        )

    assert not isinstance(raised.value, publisher.PublicationIndeterminate)
    assert not output.exists() and not output.is_symlink()
    assert _staging_names(base) == []
    assert set(path.name for path in attempt.iterdir()) == set(
        publisher.REPORT_NAMES.values()
    )
    assert adapter.open_descriptors == set()
    if fault == "directory-fsync":
        assert adapter.directory_fsyncs == 2


@pytest.mark.parametrize(
    ("fault", "expected_output"),
    [
        ("rollback-unlink", b"accepted"),
        ("rollback-fsync", None),
        ("identity-mismatch", b"foreign\n"),
    ],
)
def test_indeterminate_rollback_preserves_scene_and_forbids_success(
    tmp_path: Path, fault: str, expected_output: bytes | None
) -> None:
    base, attempt, _ = _make_evidence(tmp_path)
    output = base / "accepted.json"
    adapter = FaultIO(fault)

    with pytest.raises(publisher.PublicationIndeterminate):
        publisher.publish_acceptance(
            base, ATTEMPT_ID, INTEGRATION_SHA, output, io=adapter
        )

    if expected_output is None:
        assert not output.exists()
    elif expected_output == b"accepted":
        assert output.is_file()
        assert (
            json.loads(output.read_text(encoding="utf-8"))["result"]["status"]
            == "accepted"
        )
    else:
        assert output.read_bytes() == expected_output
    assert set(path.name for path in attempt.iterdir()) == set(
        publisher.REPORT_NAMES.values()
    )
    assert adapter.open_descriptors == set()


@pytest.mark.parametrize("kind", ["regular", "symlink", "fifo"])
def test_preexisting_output_of_every_kind_is_never_replaced(
    tmp_path: Path, kind: str
) -> None:
    base, _, _ = _make_evidence(tmp_path)
    output = base / "accepted.json"
    if kind == "regular":
        output.write_text("old\n", encoding="utf-8")
    elif kind == "symlink":
        output.symlink_to(base / "missing")
    else:
        os.mkfifo(output)
    before = output.lstat()

    with pytest.raises(publisher.AcceptanceError, match="already exists"):
        publisher.publish_acceptance(base, ATTEMPT_ID, INTEGRATION_SHA, output)

    after = output.lstat()
    assert (before.st_dev, before.st_ino, before.st_mode) == (
        after.st_dev,
        after.st_ino,
        after.st_mode,
    )


def test_realpath_attempt_output_and_uuid_boundaries_are_closed(tmp_path: Path) -> None:
    base, attempt, _ = _make_evidence(tmp_path)
    output = base / "accepted.json"
    alias = tmp_path / "evidence-alias"
    alias.symlink_to(base, target_is_directory=True)
    cases = [
        (alias, ATTEMPT_ID, alias / "accepted.json"),
        (base, ATTEMPT_ID.replace("-", ""), output),
        (base, ATTEMPT_ID, base / "other.json"),
    ]
    for candidate_base, identifier, candidate_output in cases:
        with pytest.raises(publisher.AcceptanceError):
            publisher.publish_acceptance(
                candidate_base, identifier, INTEGRATION_SHA, candidate_output
            )
    assert not output.exists()
    assert attempt.is_dir()


@pytest.mark.parametrize(
    "mutation", ["integration", "schema", "partial", "report-hash"]
)
def test_identity_schema_partial_and_report_hash_mismatches_do_not_publish(
    tmp_path: Path, mutation: str
) -> None:
    base, attempt, _ = _make_evidence(tmp_path)
    output = base / "accepted.json"
    integration_sha = INTEGRATION_SHA
    if mutation == "integration":
        integration_sha = "2" * 40
    elif mutation == "schema":
        final_path = attempt / publisher.REPORT_NAMES["final"]
        final = json.loads(final_path.read_text(encoding="utf-8"))
        final["unexpected"] = True
        _write_json(final_path, final)
    elif mutation == "partial":
        wheel_path = attempt / publisher.REPORT_NAMES["wheel"]
        wheel = json.loads(wheel_path.read_text(encoding="utf-8"))
        del wheel["result"]
        _write_json(wheel_path, wheel)
    else:
        performance_path = attempt / publisher.REPORT_NAMES["performance"]
        performance_path.write_bytes(performance_path.read_bytes() + b" \n")

    with pytest.raises(publisher.AcceptanceError):
        publisher.publish_acceptance(base, ATTEMPT_ID, integration_sha, output)

    assert not output.exists()
    assert attempt.is_dir()


def test_cli_has_no_caller_selected_schema_and_reports_indeterminate(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    with pytest.raises(SystemExit) as unknown:
        publisher.main(["--schema", str(tmp_path / "schema.json")])
    assert unknown.value.code == 2

    base, _, _ = _make_evidence(tmp_path)
    output = base / "accepted.json"
    original = publisher.PublisherIO
    publisher.PublisherIO = lambda: FaultIO("rollback-unlink")
    try:
        with pytest.raises(SystemExit) as failed:
            publisher.main(
                [
                    "--evidence-base",
                    str(base),
                    "--attempt-id",
                    ATTEMPT_ID,
                    "--integration-sha",
                    INTEGRATION_SHA,
                    "--output",
                    str(output),
                ]
            )
    finally:
        publisher.PublisherIO = original
    assert failed.value.code == 3
    assert "publication_indeterminate" in capsys.readouterr().err
