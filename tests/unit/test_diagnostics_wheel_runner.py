from __future__ import annotations

import hashlib
import importlib.util
import json
import os
import shutil
import stat
import subprocess
import sys
from pathlib import Path
from types import ModuleType
from typing import Any

import pytest


ROOT = Path(__file__).resolve().parents[2]
SCRIPT_RELATIVE = Path("scripts/test_diagnostics_wheel.sh")
SCRIPT = ROOT / SCRIPT_RELATIVE
EXPECTED_RELATIVE = Path("tests/fixtures/release/diagnostics-wheel-expected.json")
SCHEMA_RELATIVE = Path("tests/fixtures/release/diagnostics-wheel-report-schema.json")
ARTIFACT_RELATIVE = Path("tests/fixtures/artifact_layout/nodes/V07.json")
GATE_RELATIVE = Path("tests/fixtures/diagnostic_node_gates/V07.json")
REPORT_NAME = "V07-wheel-report.json"
BUILDER_IMAGE = (
    "ghcr.io/pyo3/maturin@"
    "sha256:2665227312dd1eab1c29c70a001dc8aac53155a2d048bede3b2df7f1691c8e38"
)
BUILD_TARGET = "x86_64-unknown-linux-gnu"
FORBIDDEN = [
    "node",
    "nodejs",
    "npm",
    "npx",
    "protoc",
    "perfetto",
    "trace_processor_shell",
]


FAKE_VERIFIER = r"""from __future__ import annotations

import json
import os
import shutil
import sys
from pathlib import Path


record = {
    "argv": sys.argv[1:],
    "cwd": os.getcwd(),
    "path": os.environ.get("PATH"),
    "offline": os.environ.get("TROUPE_DIAGNOSTICS_WHEEL_OFFLINE"),
    "smoke": os.environ.get("TROUPE_DIAGNOSTICS_WHEEL_SMOKE"),
    "report": os.environ.get("TROUPE_DIAGNOSTICS_WHEEL_REPORT"),
    "expected": os.environ.get("TROUPE_DIAGNOSTICS_WHEEL_EXPECTED"),
    "report_schema": os.environ.get("TROUPE_DIAGNOSTICS_WHEEL_REPORT_SCHEMA"),
    "builder_image": os.environ.get("TROUPE_DIAGNOSTICS_WHEEL_BUILDER_IMAGE"),
    "target": os.environ.get("TROUPE_DIAGNOSTICS_WHEEL_TARGET"),
    "cargo_offline": os.environ.get("CARGO_NET_OFFLINE"),
    "pip_no_index": os.environ.get("PIP_NO_INDEX"),
    "uv_offline": os.environ.get("UV_OFFLINE"),
    "http_proxy": os.environ.get("http_proxy"),
    "no_proxy": os.environ.get("no_proxy"),
    "blocked": {
        name: shutil.which(name)
        for name in (
            "node", "nodejs", "npm", "npx", "protoc", "perfetto",
            "trace_processor_shell", "uv"
        )
    },
    "cache_environment": sorted(
        name
        for name in os.environ
        if name in {
            "TROUPE_NPM_CACHE", "TROUPE_PLAYWRIGHT_CACHE", "TROUPE_PERFETTO_CACHE"
        } or name.casefold().startswith("npm_config_")
    ),
}
Path(os.environ["TROUPE_V07_FAKE_LOG"]).write_text(
    json.dumps(record, sort_keys=True), encoding="utf-8"
)
failure = int(os.environ.get("TROUPE_V07_FAKE_FAILURE", "0"))
if failure:
    raise SystemExit(failure)
report = Path(record["report"])
payload = (json.dumps({"fake": "complete"}, sort_keys=True, separators=(",", ":")) + "\n").encode()
descriptor = os.open(report, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
try:
    os.write(descriptor, payload)
    os.fsync(descriptor)
finally:
    os.close(descriptor)
"""


FAKE_DOCKER = r"""#!/usr/bin/env python3
from __future__ import annotations

import json
import os
import subprocess
import sys
from pathlib import Path


Path(os.environ["TROUPE_V07_FAKE_DOCKER_LOG"]).write_text(
    json.dumps(sys.argv[1:], separators=(",", ":")), encoding="utf-8"
)
repository = Path(os.environ["TROUPE_V07_FAKE_REPOSITORY"])
completed = subprocess.run(
    [
        sys.executable,
        str(repository / "scripts/verify_wheel.py"),
        "--build",
        "--release",
        "--target",
        "x86_64-unknown-linux-gnu",
        "--manylinux",
        "2_17",
    ],
    cwd=repository,
    env=os.environ,
    check=False,
)
raise SystemExit(completed.returncode)
"""


def load_json(path: Path) -> dict[str, Any]:
    value = json.loads(path.read_text(encoding="utf-8"))
    assert isinstance(value, dict)
    return value


def load_verifier() -> ModuleType:
    path = ROOT / "scripts/verify_wheel.py"
    spec = importlib.util.spec_from_file_location("_troupe_v07_verify_wheel", path)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def sandbox(tmp_path: Path) -> tuple[Path, Path, Path, dict[str, str], Path]:
    repository = (tmp_path / "repository").resolve()
    script = repository / SCRIPT_RELATIVE
    script.parent.mkdir(parents=True)
    shutil.copy2(SCRIPT, script)
    script.chmod(0o755)
    verifier = repository / "scripts/verify_wheel.py"
    verifier.write_text(FAKE_VERIFIER, encoding="utf-8")
    for relative in (EXPECTED_RELATIVE, SCHEMA_RELATIVE):
        path = repository / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text("{}\n", encoding="ascii")

    tools = (tmp_path / "tools").resolve()
    tools.mkdir()
    docker = tools / "docker"
    docker.write_text(FAKE_DOCKER, encoding="utf-8")
    docker.chmod(0o755)
    for name in (*FORBIDDEN, "uv"):
        executable = tools / name
        executable.write_text("#!/bin/sh\nexit 97\n", encoding="ascii")
        executable.chmod(0o755)

    gate = (tmp_path / "gate").resolve()
    gate.mkdir()
    cargo_home = (tmp_path / "cargo-home").resolve()
    (cargo_home / "registry").mkdir(parents=True)
    sentinel = gate / "caller-owned"
    sentinel.write_text("preserve\n", encoding="ascii")
    log = (tmp_path / "fake-verifier.json").resolve()
    docker_log = (tmp_path / "fake-docker.json").resolve()
    environment = dict(os.environ)
    environment.update(
        {
            "PATH": f"{tools}:{environment.get('PATH', '')}",
            "CARGO_HOME": str(cargo_home),
            "TROUPE_GATE_TMP": str(gate),
            "TROUPE_V07_FAKE_LOG": str(log),
            "TROUPE_V07_FAKE_DOCKER_LOG": str(docker_log),
            "TROUPE_V07_FAKE_REPOSITORY": str(repository),
            "TROUPE_NPM_CACHE": str(tmp_path / "npm-cache"),
            "TROUPE_PLAYWRIGHT_CACHE": str(tmp_path / "browser-cache"),
            "TROUPE_PERFETTO_CACHE": str(tmp_path / "perfetto-cache"),
            "NpM_CoNfIg_CaChE": str(tmp_path / "implicit-npm-cache"),
        }
    )
    return repository, gate, sentinel, environment, log


def run(
    repository: Path,
    gate: Path,
    environment: dict[str, str],
    *,
    report: Path | None = None,
    arguments: list[str] | None = None,
) -> subprocess.CompletedProcess[str]:
    target = gate / REPORT_NAME if report is None else report
    effective = (
        ["--offline", "--smoke", "active,archive", "--report", str(target)]
        if arguments is None
        else arguments
    )
    return subprocess.run(
        [str(repository / SCRIPT_RELATIVE), *effective],
        cwd=repository,
        env=environment,
        check=False,
        capture_output=True,
        text=True,
        timeout=30,
    )


def test_checked_contract_and_node_descriptors_are_exact() -> None:
    expected = load_json(ROOT / EXPECTED_RELATIVE)
    assert expected["schema"] == "troupe.diagnostics.wheel-expected.v1"
    assert expected["build_system"] == {
        "requires": ["maturin==1.14.1"],
        "build_backend": "maturin",
    }
    assert expected["builder_image"] == BUILDER_IMAGE
    assert expected["manylinux"] == "2_17"
    assert expected["target"] == BUILD_TARGET
    assert expected["smoke_modes"] == ["active", "archive"]
    assert expected["forbidden_tools"] == FORBIDDEN
    assert expected["size_is_informational"] is True
    assert load_json(ROOT / ARTIFACT_RELATIVE) == {
        "state": "realized",
        "introduced": [
            "scripts/test_diagnostics_wheel.sh",
            "tests/fixtures/release/diagnostics-wheel-expected.json",
            "tests/fixtures/release/diagnostics-wheel-report-schema.json",
            "tests/release/diagnostics_wheel_smoke.py",
            "tests/unit/test_diagnostics_wheel_runner.py",
        ],
        "modified": ["scripts/verify_wheel.py"],
        "removed": [],
        "generated": [],
    }
    assert load_json(ROOT / GATE_RELATIVE) == {
        "state": "realized",
        "argv": [["pytest", "-q", "tests/unit/test_diagnostics_wheel_runner.py"]],
        "env": {},
        "maturin_features": [],
        "cache_requirements": [],
        "exclusive_resources": [],
    }
    schema = load_json(ROOT / SCHEMA_RELATIVE)
    assert schema["additionalProperties"] is False
    assert schema["properties"]["exporter_size"]["properties"]["hard_limit"] == {
        "const": False
    }


def test_success_dispatches_one_offline_build_and_preserves_only_the_report(
    tmp_path: Path,
) -> None:
    repository, gate, sentinel, environment, log = sandbox(tmp_path)
    result = run(repository, gate, environment)

    assert result.returncode == 0, result.stderr
    report = gate / REPORT_NAME
    assert report.read_bytes() == b'{"fake":"complete"}\n'
    assert sentinel.read_text(encoding="ascii") == "preserve\n"
    assert not list(gate.glob("troupe-v07.*"))
    record = load_json(log)
    assert record["argv"] == [
        "--build",
        "--release",
        "--target",
        BUILD_TARGET,
        "--manylinux",
        "2_17",
    ]
    assert record["cwd"] == str(repository)
    assert record["offline"] == "1"
    assert record["smoke"] == "active,archive"
    assert record["report"] == str(report)
    assert record["expected"] == str(repository / EXPECTED_RELATIVE)
    assert record["report_schema"] == str(repository / SCHEMA_RELATIVE)
    assert record["builder_image"] == BUILDER_IMAGE
    assert record["target"] == BUILD_TARGET
    assert record["cargo_offline"] == "true"
    assert record["pip_no_index"] == "1"
    assert record["uv_offline"] == "1"
    assert record["http_proxy"] == "http://127.0.0.1:9/"
    assert record["no_proxy"] == "127.0.0.1,localhost"
    assert set(record["blocked"]) == {*FORBIDDEN, "uv"}
    assert set(record["blocked"].values()) == {None}
    assert record["cache_environment"] == []
    assert len(record["path"].split(os.pathsep)) == 1
    docker_arguments = json.loads(
        Path(environment["TROUPE_V07_FAKE_DOCKER_LOG"]).read_text(encoding="utf-8")
    )
    assert docker_arguments[:2] == ["run", "--rm"]
    assert docker_arguments[docker_arguments.index("--network") + 1] == "none"
    assert docker_arguments[docker_arguments.index("--pull") + 1] == "never"
    assert docker_arguments.count(BUILDER_IMAGE) == 1
    mounts = [
        docker_arguments[index + 1]
        for index, value in enumerate(docker_arguments)
        if value == "--mount"
    ]
    assert f"type=bind,src={repository},dst={repository},readonly" in mounts
    assert f"type=bind,src={gate},dst={gate}" in mounts
    assert any(mount.endswith("dst=/root/.cargo/registry,readonly") for mount in mounts)
    container_script = docker_arguments[docker_arguments.index("-c") + 1]
    assert "printf 'int main(void) { return 0; }" in container_script
    assert "rustc --target x86_64-unknown-linux-gnu" in container_script
    assert "cargo metadata" in container_script
    container_path = next(
        value.removeprefix("PATH=")
        for value in docker_arguments
        if value.startswith("PATH=")
    )
    assert "/opt/rh/devtoolset-10/root/usr/bin" in container_path.split(os.pathsep)
    assert "/usr/local/bin" not in container_path.split(os.pathsep)
    summary = json.loads(result.stdout)
    assert summary == {
        "schema": "troupe.diagnostics.wheel-runner.v1",
        "result": "passed",
        "report": REPORT_NAME,
        "report_sha256": hashlib.sha256(report.read_bytes()).hexdigest(),
    }


@pytest.mark.parametrize("kind", ["regular", "symlink"])
def test_preexisting_report_is_never_replaced_or_removed(
    tmp_path: Path, kind: str
) -> None:
    repository, gate, sentinel, environment, log = sandbox(tmp_path)
    report = gate / REPORT_NAME
    if kind == "regular":
        report.write_text("accepted\n", encoding="ascii")
        expected = b"accepted\n"
    else:
        target = gate / "accepted-target"
        target.write_text("accepted\n", encoding="ascii")
        report.symlink_to(target.name)
        expected = target.read_bytes()

    result = run(repository, gate, environment)

    assert result.returncode == 1
    assert "already exists" in result.stderr
    assert not log.exists()
    assert sentinel.read_text(encoding="ascii") == "preserve\n"
    if kind == "regular":
        assert report.read_bytes() == expected
    else:
        assert report.is_symlink() and report.resolve().read_bytes() == expected


def test_verifier_failure_cleans_only_owned_temporary_state(tmp_path: Path) -> None:
    repository, gate, sentinel, environment, log = sandbox(tmp_path)
    environment["TROUPE_V07_FAKE_FAILURE"] = "23"

    result = run(repository, gate, environment)

    assert result.returncode == 1
    assert "wheel verifier exited 23" in result.stderr
    assert log.is_file()
    assert not (gate / REPORT_NAME).exists()
    assert sentinel.read_text(encoding="ascii") == "preserve\n"
    assert not list(gate.glob("troupe-v07.*"))


@pytest.mark.parametrize(
    "arguments",
    [
        [],
        ["--offline"],
        ["--offline", "--smoke", "archive,active", "--report", "/tmp/report"],
        ["--smoke", "active,archive", "--offline", "--report", "/tmp/report"],
    ],
)
def test_argument_shape_is_closed(tmp_path: Path, arguments: list[str]) -> None:
    repository, gate, sentinel, environment, log = sandbox(tmp_path)
    result = run(repository, gate, environment, arguments=arguments)
    assert result.returncode == 1
    assert not log.exists()
    assert sentinel.read_text(encoding="ascii") == "preserve\n"


def test_report_must_be_the_exact_gate_root_child(tmp_path: Path) -> None:
    repository, gate, sentinel, environment, log = sandbox(tmp_path)
    outside = (tmp_path / REPORT_NAME).resolve()
    nested = gate / "nested" / REPORT_NAME
    nested.parent.mkdir()
    for report in (outside, nested, Path(REPORT_NAME)):
        result = run(repository, gate, environment, report=report)
        assert result.returncode == 1
    assert not log.exists()
    assert sentinel.read_text(encoding="ascii") == "preserve\n"


def test_verifier_report_publication_is_atomic_create_new(tmp_path: Path) -> None:
    verifier = load_verifier()
    report = (tmp_path / "report.json").resolve()
    value = {"schema": "test", "result": {"status": "passed"}}

    verifier._publish_diagnostics_report(report, value)

    expected = json.dumps(value, sort_keys=True, separators=(",", ":")).encode() + b"\n"
    assert report.read_bytes() == expected
    assert stat.S_ISREG(report.lstat().st_mode)
    assert not list(tmp_path.glob(".report.json.*.tmp"))
    with pytest.raises(verifier.VerificationError, match="already exists"):
        verifier._publish_diagnostics_report(report, {"replacement": True})
    assert report.read_bytes() == expected


def test_elf_needed_parser_rejects_non_elf_input() -> None:
    verifier = load_verifier()
    with pytest.raises(verifier.VerificationError, match="ELF"):
        verifier._elf_needed(b"not an elf")


def test_diagnostics_smoke_accepts_only_the_runner_isolated_path(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    verifier = load_verifier()
    child = tmp_path / "child"
    tool_bin = tmp_path / "isolated-tools"
    expected_path = f"{child}/bin:{tool_bin}"
    monkeypatch.setenv("PATH", str(tool_bin))

    def which(name: str, *, path: str) -> str | None:
        assert path == expected_path
        if name == "uv":
            return None
        assert name == "troupe"
        return str(child / "bin" / "troupe")

    monkeypatch.setattr(verifier.shutil, "which", which)
    verifier._validate_smoke_tools(
        child,
        {"PATH": expected_path},
        diagnostics=True,
    )


def test_report_assembly_matches_the_closed_checked_schema(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    verifier = load_verifier()
    expected, expected_payload = verifier._load_json_object(
        ROOT / EXPECTED_RELATIVE, "expected"
    )
    schema, schema_payload = verifier._load_json_object(
        ROOT / SCHEMA_RELATIVE, "schema"
    )
    identity = {
        "actor_design_sha256": "1" * 64,
        "diagnostics_design_sha256": "2" * 64,
        "plan_sha256": "3" * 64,
        "validator_sha256": "4" * 64,
        "review_record_sha256": "5" * 64,
        "integration_sha": "6" * 40,
    }
    monkeypatch.setattr(verifier, "_diagnostics_identity", lambda: identity)
    monkeypatch.setattr(
        verifier,
        "_sdist_observation",
        lambda _path: {
            "filename": "troupe-0.1.0.tar.gz",
            "sha256": "7" * 64,
            "bytes": 100,
            "regular_members": 10,
        },
    )

    environment = (tmp_path / "venv").resolve()
    package = environment / "lib/python/site-packages/troupe"
    package.mkdir(parents=True)
    wrapper = package / "__init__.py"
    native_file = package / "_runtime.abi3.so"
    wrapper.write_text("", encoding="ascii")
    native_file.write_bytes(b"native")
    native_hash = hashlib.sha256(b"native").hexdigest()
    native_path = "troupe/_runtime.abi3.so"
    members = []
    for index, path in enumerate(expected["wheel_members"]):
        actual = native_path if path == "troupe/<native>" else path
        members.append({"path": actual, "sha256": f"{index + 1:064x}", "bytes": index})
    wheel = {
        "filename": "troupe-0.1.0-cp310-abi3-manylinux_2_17_x86_64.whl",
        "sha256": "8" * 64,
        "bytes": 1000,
        "members": sorted(members, key=lambda row: row["path"]),
        "native_path": native_path,
        "native_sha256": native_hash,
        "native_bytes": len(b"native"),
        "elf_needed": ["libc.so.6"],
    }
    ui = {
        "html_bytes": 10,
        "html_sha256": "9" * 64,
        "assets": [{"path": "/assets/app.js", "sha256": "a" * 64, "bytes": 20}],
    }
    smoke = {
        "modes": ["active", "archive"],
        "run_id": "00000000-0000-4000-8000-000000000007",
        "installed": {
            "environment": str(environment),
            "troupe_file": str(wrapper),
            "native_file": str(native_file),
            "native_bytes": len(b"native"),
            "native_sha256": native_hash,
        },
        "active": {"status": "passed", "ui": ui},
        "archive": {
            "status": "passed",
            "ui": ui,
            "trace_bytes": 30,
            "trace_sha256": "b" * 64,
        },
        "forbidden_tools": FORBIDDEN,
        "production_imports": 1,
    }
    configuration = {
        "offline": True,
        "smoke": ("active", "archive"),
        "builder_image": BUILDER_IMAGE,
        "target": BUILD_TARGET,
    }

    report = verifier._assemble_diagnostics_report(
        configuration,
        expected,
        expected_payload,
        schema,
        schema_payload,
        tmp_path / "unused.tar.gz",
        wheel,
        wheel,
        smoke,
    )

    verifier._validate_report_schema(report, schema)
    result = report["result"]
    material = dict(report)
    del material["result"]
    assert result == {
        "status": "passed",
        "result_sha256": hashlib.sha256(verifier._canonical_json(material)).hexdigest(),
    }
    invalid = dict(report)
    invalid["unexpected"] = True
    with pytest.raises(verifier.VerificationError, match="extra fields"):
        verifier._validate_report_schema(invalid, schema)
