from __future__ import annotations

import base64
import hashlib
import json
import os
import shutil
import sys
import zipfile
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

import pytest


ROOT = Path(__file__).resolve().parents[2]
SUPPORT = ROOT / "tests" / "support"
sys.path.insert(0, str(SUPPORT))

import diagnostic_gate as gate  # noqa: E402


def _record_hash(data: bytes) -> str:
    digest = base64.urlsafe_b64encode(hashlib.sha256(data).digest()).rstrip(b"=")
    return f"sha256={digest.decode('ascii')}"


def _write_wheel(
    directory: Path,
    *,
    native: bytes = b"current native module",
    member: str = "troupe/_runtime.abi3.so",
    recorded_native: bytes | None = None,
    extra_member: str | None = None,
    symlink_member: str | None = None,
    dist_info: str = "troupe-0.1.0.dist-info",
) -> Path:
    directory.mkdir(parents=True, exist_ok=True)
    wheel = directory / "troupe-0.1.0-cp310-abi3-linux_x86_64.whl"
    record = f"{dist_info}/RECORD"
    metadata_name = f"{dist_info}/METADATA"
    metadata = b"Metadata-Version: 2.1\nName: troupe\nVersion: 0.1.0\n"
    recorded = native if recorded_native is None else recorded_native
    rows = [
        f"{member},{_record_hash(recorded)},{len(recorded)}",
        f"{metadata_name},{_record_hash(metadata)},{len(metadata)}",
        f"{record},,",
    ]
    with zipfile.ZipFile(wheel, "w") as archive:
        archive.writestr(member, native)
        archive.writestr(metadata_name, metadata)
        if extra_member is not None:
            archive.writestr(extra_member, b"extra")
            rows.insert(-1, f"{extra_member},{_record_hash(b'extra')},5")
        if symlink_member is not None:
            link = zipfile.ZipInfo(symlink_member)
            link.create_system = 3
            link.external_attr = (0o120777 << 16) | 0xA000
            archive.writestr(link, b"target")
            rows.insert(-1, f"{symlink_member},{_record_hash(b'target')},6")
        archive.writestr(record, "\n".join(rows) + "\n")
    return wheel


def _workspace(tmp_path: Path, name: str = "repository") -> gate.OwnedWorkspace:
    repository = tmp_path / name
    repository.mkdir()
    return gate.create_owned_workspace(repository, "F03")


def _origin_payload(
    workspace: gate.OwnedWorkspace,
    native: gate.NativeWheel,
    *,
    runtime: Path | None = None,
) -> dict[str, str]:
    python = workspace.venv / "bin/python"
    console = workspace.venv / "bin/troupe"
    package = workspace.venv / "lib/python3.12/site-packages/troupe"
    runtime = package / Path(native.member).name if runtime is None else runtime
    for path in (python, console, package / "__init__.py", runtime):
        path.parent.mkdir(parents=True, exist_ok=True)
        if not path.exists():
            path.write_bytes(b"#!/bin/sh\n" if path in (python, console) else b"package\n")
    runtime.write_bytes(native.data)
    return {
        "sys_executable": str(python),
        "console_script": str(console),
        "troupe_file": str(package / "__init__.py"),
        "runtime_file": str(runtime),
    }


def test_workspace_environment_is_fully_owned_and_ignores_shared_caches(
    tmp_path: Path,
) -> None:
    workspace = _workspace(tmp_path)
    try:
        environment = gate.gate_environment(
            workspace,
            {
                "HOME": "/shared/home",
                "PATH": "/tools:/usr/bin:/bin",
                "UV_CACHE_DIR": "/shared/uv",
                "UV_PROJECT_ENVIRONMENT": "/shared/venv",
                "VIRTUAL_ENV": "/shared/active",
                "CARGO_TARGET_DIR": "/shared/target",
                "NPM_CONFIG_CACHE": "/shared/npm",
                "PYTHONHOME": "/shared/python",
                "PYTHONPATH": "/shared/source",
            },
        )
        assert environment["HOME"] == str(workspace.home)
        assert environment["TMPDIR"] == str(workspace.tmp)
        assert environment["UV_CACHE_DIR"] == str(workspace.uv_cache)
        assert environment["UV_PROJECT_ENVIRONMENT"] == str(workspace.venv)
        assert environment["CARGO_TARGET_DIR"] == str(workspace.target)
        assert environment["NPM_CONFIG_CACHE"] == str(workspace.npm_cache)
        assert environment["PYO3_PYTHON"] == str(workspace.venv / "bin/python")
        assert "VIRTUAL_ENV" not in environment
        assert "PYTHONHOME" not in environment
        assert "PYTHONPATH" not in environment
        for value in (
            environment["HOME"],
            environment["TMPDIR"],
            environment["UV_CACHE_DIR"],
            environment["UV_PROJECT_ENVIRONMENT"],
            environment["CARGO_TARGET_DIR"],
            environment["NPM_CONFIG_CACHE"],
        ):
            assert Path(value).is_relative_to(workspace.root)
    finally:
        gate.cleanup_owned_workspace(workspace)


def test_managed_python_runtime_binds_home_and_precedes_inherited_loader_paths(
    tmp_path: Path,
) -> None:
    workspace = _workspace(tmp_path)
    managed = tmp_path / "managed-python"
    managed_python = managed / "bin/python3.10"
    managed_python.parent.mkdir(parents=True)
    managed_python.write_bytes(b"python")
    (managed / "lib").mkdir()
    (workspace.venv / "bin").mkdir(parents=True)
    (workspace.venv / "bin/python").symlink_to(managed_python)
    environment = {"LD_LIBRARY_PATH": "/caller/lib"}
    try:
        gate.bind_managed_python_runtime(workspace, environment)
        assert environment["LD_LIBRARY_PATH"] == f"{managed / 'lib'}{os.pathsep}/caller/lib"
        assert environment["PYTHONHOME"] == str(managed)
    finally:
        gate.cleanup_owned_workspace(workspace)


def test_managed_python_runtime_library_must_exist(tmp_path: Path) -> None:
    workspace = _workspace(tmp_path)
    managed_python = tmp_path / "managed-python/bin/python3.10"
    managed_python.parent.mkdir(parents=True)
    managed_python.write_bytes(b"python")
    (workspace.venv / "bin").mkdir(parents=True)
    (workspace.venv / "bin/python").symlink_to(managed_python)
    try:
        with pytest.raises(gate.GateError, match="managed Python library directory"):
            gate.bind_managed_python_runtime(workspace, {})
    finally:
        gate.cleanup_owned_workspace(workspace)


def test_two_repository_copies_never_share_a_writable_gate_path(tmp_path: Path) -> None:
    repositories = [tmp_path / "copy-a", tmp_path / "copy-b"]
    for repository in repositories:
        repository.mkdir()

    with ThreadPoolExecutor(max_workers=2) as pool:
        workspaces = list(
            pool.map(lambda repository: gate.create_owned_workspace(repository, "F03"), repositories)
        )
    try:
        roots = [workspace.root for workspace in workspaces]
        assert roots[0] != roots[1]
        assert roots[0].is_relative_to(repositories[0] / ".troupe-test")
        assert roots[1].is_relative_to(repositories[1] / ".troupe-test")
        writable = [set(gate.writable_paths(workspace)) for workspace in workspaces]
        assert writable[0].isdisjoint(writable[1])
    finally:
        for workspace in workspaces:
            gate.cleanup_owned_workspace(workspace)


@pytest.mark.parametrize("failure", [RuntimeError("failed"), KeyboardInterrupt()])
def test_owned_workspace_cleans_after_failure_or_interrupt(
    tmp_path: Path,
    failure: BaseException,
) -> None:
    repository = tmp_path / "repository"
    repository.mkdir()
    root: Path | None = None
    with pytest.raises(type(failure)):
        with gate.owned_workspace(repository, "F03") as workspace:
            root = workspace.root
            raise failure
    assert root is not None and not root.exists()
    assert list((repository / ".troupe-test").iterdir()) == []


def test_cleanup_refuses_identity_swap_and_never_follows_escape_symlink(tmp_path: Path) -> None:
    workspace = _workspace(tmp_path)
    held = workspace.root.with_name(f"{workspace.root.name}.held")
    victim = tmp_path / "victim"
    victim.mkdir()
    sentinel = victim / "sentinel"
    sentinel.write_text("preserve\n", encoding="utf-8")
    workspace.root.rename(held)
    workspace.root.symlink_to(victim, target_is_directory=True)

    with pytest.raises(gate.GateError, match="identity changed"):
        gate.cleanup_owned_workspace(workspace)
    assert sentinel.read_text(encoding="utf-8") == "preserve\n"

    workspace.root.unlink()
    shutil.rmtree(held)


def test_workspace_rejects_symlinked_owned_parent(tmp_path: Path) -> None:
    repository = tmp_path / "repository"
    repository.mkdir()
    outside = tmp_path / "outside"
    outside.mkdir()
    (repository / ".troupe-test").symlink_to(outside, target_is_directory=True)
    with pytest.raises(gate.GateError, match="regular directory"):
        gate.create_owned_workspace(repository, "F03")
    assert list(outside.iterdir()) == []


def test_features_are_closed_unique_and_present_in_current_manifest(tmp_path: Path) -> None:
    manifest = tmp_path / "Cargo.toml"
    manifest.write_text(
        "[features]\ndefault = []\nagent-test-support = []\n",
        encoding="utf-8",
    )
    assert gate.validated_features(manifest, ("agent-test-support",)) == "agent-test-support"
    with pytest.raises(gate.GateError, match="duplicate"):
        gate.validated_features(manifest, ("agent-test-support", "agent-test-support"))
    with pytest.raises(gate.GateError, match="unknown"):
        gate.validated_features(manifest, ("diagnostics-test-support",))


def test_empty_feature_descriptor_executes_without_installing_a_wheel(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    import artifact_layout

    workspace = _workspace(tmp_path)
    descriptor = artifact_layout.GateDescriptor(
        state="realized",
        argv=(("scripts/run_diagnostic_node_gate.sh", "D11"),),
        env={},
        maturin_features=(),
        cache_requirements=(),
        exclusive_resources=(),
    )
    monkeypatch.setattr(
        artifact_layout,
        "load_gate_descriptors",
        lambda _repository: {"D11": descriptor},
    )
    monkeypatch.setattr(gate, "_current_checkout", lambda _repository, _environment: None)

    def reject_native_install(*_args: object, **_kwargs: object) -> None:
        pytest.fail("direct gate attempted to install a native wheel")

    monkeypatch.setattr(gate, "_install_native_wheel", reject_native_install)
    try:
        gate._execute_gate(workspace.repository, "D11", workspace)
    finally:
        gate.cleanup_owned_workspace(workspace)


def test_cargo_json_artifacts_must_all_resolve_inside_owned_target(tmp_path: Path) -> None:
    target = tmp_path / "target"
    artifact = target / "debug/lib_runtime.so"
    out_dir = target / "debug/build/runtime/out"
    artifact.parent.mkdir(parents=True)
    out_dir.mkdir(parents=True)
    artifact.write_bytes(b"native")
    messages = "\n".join(
        (
            json.dumps(
                {
                    "reason": "compiler-artifact",
                    "filenames": [str(artifact)],
                    "executable": None,
                }
            ),
            json.dumps({"reason": "build-script-executed", "out_dir": str(out_dir)}),
        )
    )
    gate.validate_cargo_artifacts(messages, target)

    foreign = tmp_path / "foreign/lib_runtime.so"
    foreign.parent.mkdir()
    foreign.write_bytes(b"foreign")
    escaped = json.dumps(
        {"reason": "compiler-artifact", "filenames": [str(foreign)], "executable": None}
    )
    with pytest.raises(gate.GateError, match="outside owned target"):
        gate.validate_cargo_artifacts(escaped, target)


def test_cargo_json_rejects_symlink_artifact_that_resolves_outside_target(tmp_path: Path) -> None:
    target = tmp_path / "target"
    target.mkdir()
    foreign = tmp_path / "foreign.so"
    foreign.write_bytes(b"foreign")
    linked = target / "linked.so"
    linked.symlink_to(foreign)
    message = json.dumps(
        {"reason": "compiler-artifact", "filenames": [str(linked)], "executable": None}
    )
    with pytest.raises(gate.GateError, match="outside owned target"):
        gate.validate_cargo_artifacts(message, target)


def test_wheel_selection_rejects_zero_multiple_foreign_and_stale_files(tmp_path: Path) -> None:
    wheel_dir = tmp_path / "wheels"
    wheel_dir.mkdir()
    with pytest.raises(gate.GateError, match="exactly one wheel"):
        gate.select_built_wheel(wheel_dir, 0)

    first = _write_wheel(wheel_dir)
    started = first.stat().st_mtime_ns
    second = wheel_dir / "foreign.whl"
    second.write_bytes(b"foreign")
    with pytest.raises(gate.GateError, match="exactly one wheel"):
        gate.select_built_wheel(wheel_dir, started)
    second.unlink()

    os.utime(first, ns=(started - 1, started - 1))
    with pytest.raises(gate.GateError, match="stale"):
        gate.select_built_wheel(wheel_dir, started)

    first.rename(wheel_dir / "foreign-1.0.0-py3-none-any.whl")
    with pytest.raises(gate.GateError, match="foreign wheel"):
        gate.select_built_wheel(wheel_dir, 0)


def test_wheel_record_binds_the_single_native_member(tmp_path: Path) -> None:
    native = b"native bytes"
    inspected = gate.inspect_wheel(_write_wheel(tmp_path / "wheels", native=native))
    assert inspected.member == "troupe/_runtime.abi3.so"
    assert inspected.sha256 == hashlib.sha256(native).hexdigest()
    assert inspected.data == native

    mismatch = _write_wheel(
        tmp_path / "mismatch",
        native=native,
        recorded_native=b"some other bytes",
    )
    with pytest.raises(gate.GateError, match="RECORD hash"):
        gate.inspect_wheel(mismatch)

    foreign = _write_wheel(tmp_path / "foreign", dist_info="foreign-1.0.0.dist-info")
    with pytest.raises(gate.GateError, match="distribution identity"):
        gate.inspect_wheel(foreign)


@pytest.mark.parametrize(
    ("kwargs", "message"),
    [
        ({"member": "../troupe/_runtime.abi3.so"}, "unsafe member"),
        ({"extra_member": "/absolute"}, "unsafe member"),
        ({"symlink_member": "troupe/link"}, "regular files"),
    ],
)
def test_wheel_rejects_path_traversal_absolute_and_symlink_members(
    tmp_path: Path,
    kwargs: dict[str, str],
    message: str,
) -> None:
    wheel = _write_wheel(tmp_path / "wheels", **kwargs)
    with pytest.raises(gate.GateError, match=message):
        gate.inspect_wheel(wheel)


def test_installed_origin_is_current_hash_bound_and_inside_fresh_venv(tmp_path: Path) -> None:
    workspace = _workspace(tmp_path)
    try:
        wheel = gate.inspect_wheel(_write_wheel(workspace.wheels))
        payload = _origin_payload(workspace, wheel)
        runtime = Path(payload["runtime_file"])
        started = runtime.stat().st_mtime_ns
        gate.validate_installed_origin(payload, workspace, wheel, started)

        runtime.write_bytes(b"foreign module")
        with pytest.raises(gate.GateError, match="does not match wheel"):
            gate.validate_installed_origin(payload, workspace, wheel, started)
    finally:
        gate.cleanup_owned_workspace(workspace)


def test_installed_origin_rejects_foreign_paths_and_stale_native_module(tmp_path: Path) -> None:
    workspace = _workspace(tmp_path)
    try:
        wheel = gate.inspect_wheel(_write_wheel(workspace.wheels))
        foreign = tmp_path / "shared-site/troupe/_runtime.abi3.so"
        payload = _origin_payload(workspace, wheel, runtime=foreign)
        started = foreign.stat().st_mtime_ns
        with pytest.raises(gate.GateError, match="outside fresh venv"):
            gate.validate_installed_origin(payload, workspace, wheel, started)

        payload = _origin_payload(workspace, wheel)
        runtime = Path(payload["runtime_file"])
        os.utime(runtime, ns=(started - 1, started - 1))
        with pytest.raises(gate.GateError, match="stale"):
            gate.validate_installed_origin(payload, workspace, wheel, started)
    finally:
        gate.cleanup_owned_workspace(workspace)


def test_descriptor_command_resolution_uses_fresh_tools_and_rejects_escape(tmp_path: Path) -> None:
    workspace = _workspace(tmp_path)
    try:
        for name in ("python", "pytest", "troupe"):
            executable = workspace.venv / "bin" / name
            executable.parent.mkdir(parents=True, exist_ok=True)
            executable.write_text("#!/bin/sh\n", encoding="utf-8")
        assert gate.resolve_command(("python", "-m", "pytest"), workspace, workspace.repository)[0] == str(
            workspace.venv / "bin/python"
        )
        assert gate.resolve_command(("pytest", "-q"), workspace, workspace.repository)[0] == str(
            workspace.venv / "bin/pytest"
        )
        assert gate.resolve_command(("troupe", "--help"), workspace, workspace.repository)[0] == str(
            workspace.venv / "bin/troupe"
        )
        with pytest.raises(gate.GateError, match="escapes repository"):
            gate.resolve_command(("../outside",), workspace, workspace.repository)

        outside = tmp_path / "outside"
        outside.write_text("#!/bin/sh\n", encoding="utf-8")
        linked = workspace.repository / "linked-tool"
        linked.symlink_to(outside)
        with pytest.raises(gate.GateError, match="escapes repository"):
            gate.resolve_command(("./linked-tool",), workspace, workspace.repository)

        with pytest.raises(gate.GateError, match="must not execute a shell"):
            gate.resolve_command(("bash", "-c", "true"), workspace, workspace.repository)
        with pytest.raises(gate.GateError, match="inline Python"):
            gate.resolve_command(("python", "-c", "pass"), workspace, workspace.repository)
    finally:
        gate.cleanup_owned_workspace(workspace)


def test_descriptor_argument_expansion_uses_constructed_child_environment() -> None:
    environment = gate._descriptor_environment(
        "F03",
        {"PATH": "/usr/bin:/bin", "TROUPE_GATE_TMP": "/external/gate"},
        {"TROUPE_GATE_TMP": "required"},
        (),
    )
    assert gate._expand_argument(
        "--report=${TROUPE_GATE_TMP:?}/report.json",
        environment,
    ) == "--report=/external/gate/report.json"


@pytest.mark.parametrize("value", [None, ""])
def test_descriptor_argument_expansion_rejects_missing_or_empty_value(
    value: str | None,
) -> None:
    environment = {} if value is None else {"TROUPE_GATE_TMP": value}
    with pytest.raises(gate.GateError, match="requires missing environment: TROUPE_GATE_TMP"):
        gate._expand_argument("${TROUPE_GATE_TMP:?}/report.json", environment)


@pytest.mark.parametrize(
    "argument",
    [
        "${TROUPE_GATE_TMP}",
        "${TROUPE_GATE_TMP:-fallback}",
        "${lower_case:?}",
        "${1INVALID:?}",
    ],
)
def test_descriptor_argument_expansion_rejects_unsupported_reference(argument: str) -> None:
    with pytest.raises(gate.GateError, match="unsupported environment reference"):
        gate._expand_argument(argument, {"TROUPE_GATE_TMP": "/external/gate"})


@pytest.mark.parametrize(
    ("cache", "name"),
    [
        ("npm", "TROUPE_NPM_CACHE"),
        ("perfetto", "TROUPE_PERFETTO_CACHE"),
        ("playwright", "TROUPE_PLAYWRIGHT_CACHE"),
    ],
)
def test_descriptor_cache_requirement_injects_exact_caller_value(
    cache: str,
    name: str,
) -> None:
    environment = gate._descriptor_environment(
        "F03",
        {name: f"/readonly/{cache}"},
        {},
        (cache,),
    )
    assert environment[name] == f"/readonly/{cache}"


@pytest.mark.parametrize("value", [None, ""])
def test_descriptor_cache_requirement_rejects_missing_or_empty_caller_value(
    value: str | None,
) -> None:
    caller = {} if value is None else {"TROUPE_NPM_CACHE": value}
    with pytest.raises(gate.GateError, match="required gate cache is missing: TROUPE_NPM_CACHE"):
        gate._descriptor_environment("F03", caller, {}, ("npm",))


@pytest.mark.parametrize(
    "name",
    ["TROUPE_DIAGNOSTICS_EVIDENCE", "TROUPE_FINAL_ATTEMPT_ID"],
)
def test_ordinary_node_rejects_persistent_evidence_environment(name: str) -> None:
    with pytest.raises(gate.GateError, match="must not request persistent evidence"):
        gate._descriptor_environment(
            "F03",
            {name: "/persistent/evidence"},
            {name: "required"},
            (),
        )


@pytest.mark.parametrize("node_id", ["V03", "V16"])
def test_evidence_nodes_may_request_persistent_environment(node_id: str) -> None:
    environment = gate._descriptor_environment(
        node_id,
        {"TROUPE_DIAGNOSTICS_EVIDENCE": "/persistent/evidence"},
        {"TROUPE_DIAGNOSTICS_EVIDENCE": "required"},
        (),
    )
    assert environment["TROUPE_DIAGNOSTICS_EVIDENCE"] == "/persistent/evidence"
