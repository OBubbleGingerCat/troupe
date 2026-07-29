from __future__ import annotations

import base64
import contextlib
import csv
import hashlib
import importlib.util
import importlib.metadata
import io
import json
import os
import shutil
import stat
import subprocess
import sys
import sysconfig
import tarfile
import warnings
import zipfile
from collections.abc import Generator
from pathlib import Path
from types import ModuleType
from typing import Any

import pytest
from wheel.wheelfile import WheelFile

try:
    import tomllib
except ModuleNotFoundError:
    import tomli as tomllib


ROOT = Path(__file__).resolve().parents[2]
PACKAGE = ROOT / "src" / "troupe"

EXPECTED_WRAPPER = (
    b"from ._runtime import Production as Production\n"
    b"\n"
    b'__all__ = ["Production"]\n'
)
EXPECTED_STUB = (
    b"from typing_extensions import disjoint_base\n"
    b"\n"
    b"@disjoint_base\n"
    b"class Production:\n"
    b"    def __new__(cls, args: list[str], /) -> Production: ...\n"
    b"    async def start(self) -> None: ...\n"
    b"    async def scene(self) -> None: ...\n"
    b"    async def stop(self) -> None: ...\n"
    b"\n"
    b'__all__ = ["Production"]\n'
)
EXPECTED_ENTRY_POINTS = b"[console_scripts]\ntroupe = troupe._runtime:main\n"


def _toml(path: Path) -> dict[str, Any]:
    return tomllib.loads(path.read_text(encoding="utf-8"))


def _verifier() -> ModuleType:
    script = ROOT / "scripts" / "verify_wheel.py"
    spec = importlib.util.spec_from_file_location("_troupe_verify_wheel_test", script)
    assert spec is not None
    assert spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


def _add_tar_bytes(archive: tarfile.TarFile, name: str, data: bytes) -> None:
    info = tarfile.TarInfo(name)
    info.size = len(data)
    archive.addfile(info, io.BytesIO(data))


def _expanded_tags(tag: str) -> list[str]:
    python_tag, abi_tag, platform_tag = tag.split("-", maxsplit=2)
    return [
        f"{python}-{abi}-{platform}"
        for python in python_tag.split(".")
        for abi in abi_tag.split(".")
        for platform in platform_tag.split(".")
    ]


def _record_bytes(
    files: dict[str, bytes],
    record_name: str,
    mutation: str | None,
) -> bytes:
    stream = io.StringIO()
    writer = csv.writer(stream, lineterminator="\n")
    rows: list[list[str]] = []
    for name, data in files.items():
        digest = base64.urlsafe_b64encode(hashlib.sha256(data).digest()).rstrip(b"=")
        rows.append([name, f"sha256={digest.decode('ascii')}", str(len(data))])

    native_index = next(
        (index for index, row in enumerate(rows) if row[0].endswith(".so")),
        0,
    )
    if mutation == "wrong-size":
        rows[native_index][2] = str(int(rows[native_index][2]) + 1)
    elif mutation == "wrong-hash":
        rows[native_index][1] = "sha256=" + ("A" * 43)
    elif mutation == "phantom":
        rows.append(["phantom.py", "sha256=" + ("A" * 43), "0"])
    elif mutation == "duplicate-row":
        rows.append(rows[0].copy())
    elif mutation == "missing-member-row":
        rows.pop(native_index)
    elif mutation == "extra-column":
        rows[0].append("extra")
    elif mutation == "invalid-size":
        rows[0][2] = "not-a-size"
    elif mutation == "negative-size":
        rows[0][2] = "-1"
    elif mutation == "empty-hash":
        rows[0][1] = ""
    elif mutation == "empty-size":
        rows[0][2] = ""

    writer.writerows(rows)
    if mutation != "missing-self-row":
        if mutation == "self-nonempty":
            writer.writerow([record_name, "sha256=" + ("A" * 43), "1"])
        else:
            writer.writerow([record_name, "", ""])
    return stream.getvalue().encode("utf-8")


def _synthetic_artifacts(
    tmp_path: Path,
    *,
    extra_source_python: bool = False,
    extra_source_stub: bool = False,
    extra_sdist_python: bool = False,
    extra_sdist_stub: bool = False,
    extra_wheel_python: bool = False,
    extra_package_python: bool = False,
    source_wrapper: bytes = EXPECTED_WRAPPER,
    source_stub: bytes | None = EXPECTED_STUB,
    source_py_typed: bool = True,
    sdist_wrapper: bytes = EXPECTED_WRAPPER,
    sdist_stub: bytes | None = EXPECTED_STUB,
    sdist_py_typed: bool = True,
    sdist_unsafe: str | None = None,
    wheel_wrapper: bytes = EXPECTED_WRAPPER,
    wheel_stub: bytes | None = EXPECTED_STUB,
    wheel_py_typed: bool = True,
    native_count: int = 1,
    runtime_stem: str = "_runtime",
    extra_native: str | None = None,
    tag: str = "cp310-abi3-manylinux_2_17_x86_64",
    wheel_tags: list[str] | None = None,
    requires_dist: bool = False,
    metadata_name: str = "troupe",
    metadata_version: str = "0.1.0",
    requires_python: str = ">=3.10",
    wheel_version: str | None = "1.0",
    root_is_purelib: str = "false",
    entry_points: str = "valid",
    forbidden_file: str | None = None,
    record_mutation: str | None = None,
    duplicate_member: bool = False,
) -> tuple[Path, Path, Path]:
    source = tmp_path / "source" / "troupe"
    source.mkdir(parents=True)
    (source / "__init__.py").write_bytes(source_wrapper)
    if source_stub is not None:
        (source / "__init__.pyi").write_bytes(source_stub)
    if source_py_typed:
        (source / "py.typed").touch()
    if extra_source_python:
        (source / "nested").mkdir()
        (source / "nested" / "helper.py").write_text("VALUE = 1\n", encoding="utf-8")
    if extra_source_stub:
        (source / "helper.pyi").write_text("VALUE: int\n", encoding="utf-8")

    sdist = tmp_path / "troupe-0.1.0.tar.gz"
    sdist_root = "troupe-0.1.0/src/troupe"
    with tarfile.open(sdist, "w:gz") as archive:
        _add_tar_bytes(archive, f"{sdist_root}/__init__.py", sdist_wrapper)
        if sdist_stub is not None:
            _add_tar_bytes(archive, f"{sdist_root}/__init__.pyi", sdist_stub)
        if sdist_py_typed:
            _add_tar_bytes(archive, f"{sdist_root}/py.typed", b"")
        if extra_sdist_python:
            _add_tar_bytes(archive, f"{sdist_root}/nested/helper.py", b"VALUE = 1\n")
        if extra_sdist_stub:
            _add_tar_bytes(archive, f"{sdist_root}/helper.pyi", b"VALUE: int\n")
        if sdist_unsafe == "traversal":
            _add_tar_bytes(archive, "../escape.dat", b"unsafe\n")
        elif sdist_unsafe == "absolute":
            _add_tar_bytes(archive, "/escape.dat", b"unsafe\n")
        elif sdist_unsafe == "symlink":
            link = tarfile.TarInfo(f"{sdist_root}/linked.dat")
            link.type = tarfile.SYMTYPE
            link.linkname = "/tmp/escape.dat"
            archive.addfile(link)

    wheel = tmp_path / f"troupe-0.1.0-{tag}.whl"
    metadata = (
        "Metadata-Version: 2.4\n"
        f"Name: {metadata_name}\n"
        f"Version: {metadata_version}\n"
        f"Requires-Python: {requires_python}\n"
    )
    if requires_dist:
        metadata += "Requires-Dist: typing-extensions\n"
    resolved_wheel_tags = wheel_tags if wheel_tags is not None else _expanded_tags(tag)
    wheel_metadata = ""
    if wheel_version is not None:
        wheel_metadata += f"Wheel-Version: {wheel_version}\n"
    wheel_metadata += (
        "Generator: test\n"
        f"Root-Is-Purelib: {root_is_purelib}\n"
        + "".join(f"Tag: {wheel_tag}\n" for wheel_tag in resolved_wheel_tags)
    )
    dist_info = "troupe-0.1.0.dist-info"
    wheel_files: dict[str, bytes] = {
        "troupe/__init__.py": wheel_wrapper,
        f"{dist_info}/METADATA": metadata.encode("utf-8"),
        f"{dist_info}/WHEEL": wheel_metadata.encode("utf-8"),
    }
    if wheel_stub is not None:
        wheel_files["troupe/__init__.pyi"] = wheel_stub
    if wheel_py_typed:
        wheel_files["troupe/py.typed"] = b""
    if entry_points == "valid":
        wheel_files[f"{dist_info}/entry_points.txt"] = EXPECTED_ENTRY_POINTS
    elif entry_points == "compact":
        wheel_files[f"{dist_info}/entry_points.txt"] = (
            b"[console_scripts]\ntroupe=troupe._runtime:main\n"
        )
    elif entry_points == "wrong":
        wheel_files[f"{dist_info}/entry_points.txt"] = (
            b"[console_scripts]\ntroupe = troupe:main\n"
        )
    elif entry_points == "extra":
        wheel_files[f"{dist_info}/entry_points.txt"] = (
            EXPECTED_ENTRY_POINTS + b"other = troupe._runtime:main\n"
        )
    elif entry_points == "duplicate-key":
        wheel_files[f"{dist_info}/entry_points.txt"] = (
            EXPECTED_ENTRY_POINTS + b"troupe = troupe._runtime:main\n"
        )
    elif entry_points == "duplicate-section":
        wheel_files[f"{dist_info}/entry_points.txt"] = (
            EXPECTED_ENTRY_POINTS
            + b"[console_scripts]\ntroupe = troupe._runtime:main\n"
        )
    elif entry_points == "extra-section":
        wheel_files[f"{dist_info}/entry_points.txt"] = (
            EXPECTED_ENTRY_POINTS + b"[other]\nvalue = target\n"
        )
    elif entry_points == "wrong-section":
        wheel_files[f"{dist_info}/entry_points.txt"] = (
            b"[other]\ntroupe = troupe._runtime:main\n"
        )
    elif entry_points == "default-section":
        wheel_files[f"{dist_info}/entry_points.txt"] = (
            b"[DEFAULT]\ntroupe = troupe._runtime:main\n[console_scripts]\n"
        )
    elif entry_points == "case-key":
        wheel_files[f"{dist_info}/entry_points.txt"] = (
            b"[console_scripts]\nTROUPE = troupe._runtime:main\n"
        )
    elif entry_points == "colon-delimiter":
        wheel_files[f"{dist_info}/entry_points.txt"] = (
            b"[console_scripts]\ntroupe: troupe._runtime:main\n"
        )
    elif entry_points == "malformed":
        wheel_files[f"{dist_info}/entry_points.txt"] = b"troupe=troupe._runtime:main\n"
    elif entry_points == "invalid-utf8":
        wheel_files[f"{dist_info}/entry_points.txt"] = b"\xff"
    elif entry_points != "missing":
        raise AssertionError(f"unknown entry_points fixture: {entry_points}")
    for index in range(native_count):
        suffix = "" if index == 0 else f".extra{index}"
        wheel_files[f"troupe/{runtime_stem}{suffix}.abi3.so"] = b"native"
    if extra_native is not None:
        wheel_files[f"troupe/{extra_native}"] = b"native"
    if extra_wheel_python:
        wheel_files["troupe/nested/helper.py"] = b"VALUE = 1\n"
    if extra_package_python:
        wheel_files["other_package/__init__.py"] = b"VALUE = 1\n"
    if forbidden_file is not None:
        wheel_files[forbidden_file] = b"forbidden"

    record_name = f"{dist_info}/RECORD"
    record = _record_bytes(wheel_files, record_name, record_mutation)
    with zipfile.ZipFile(wheel, "w") as archive:
        for name, data in wheel_files.items():
            archive.writestr(name, data)
        if record_mutation != "missing-record":
            archive.writestr(record_name, record)
        if duplicate_member:
            with warnings.catch_warnings():
                warnings.simplefilter("ignore", UserWarning)
                archive.writestr("troupe/__init__.py", wheel_wrapper)

    return source, sdist, wheel


def test_runtime_package_has_exact_thin_sources() -> None:
    python_files = sorted(path.relative_to(PACKAGE).as_posix() for path in PACKAGE.rglob("*.py"))
    stub_files = sorted(path.relative_to(PACKAGE).as_posix() for path in PACKAGE.rglob("*.pyi"))

    assert python_files == ["__init__.py"]
    assert stub_files == ["__init__.pyi"]
    assert (PACKAGE / "__init__.py").read_bytes() == EXPECTED_WRAPPER
    assert (PACKAGE / "__init__.pyi").read_bytes() == EXPECTED_STUB
    assert (PACKAGE / "py.typed").is_file()


def test_python_project_metadata_and_build_configuration() -> None:
    config = _toml(ROOT / "pyproject.toml")

    assert config["build-system"] == {
        "requires": ["maturin==1.14.1"],
        "build-backend": "maturin",
    }

    project = config["project"]
    assert project["name"] == "troupe"
    assert project["requires-python"] == ">=3.10"
    assert project.get("dependencies", []) == []
    assert project["scripts"] == {"troupe": "troupe._runtime:main"}
    classifiers = set(project["classifiers"])
    assert "Programming Language :: Python :: 3 :: Only" in classifiers
    assert "Programming Language :: Python :: Implementation :: CPython" in classifiers
    assert all("PyPy" not in classifier for classifier in classifiers)
    assert all("Free Threading" not in classifier for classifier in classifiers)

    maturin = config["tool"]["maturin"]
    assert maturin == {
        "python-source": "src",
        "manifest-path": "rust/Cargo.toml",
        "module-name": "troupe._runtime",
        "locked": True,
        "exclude": ["**/__pycache__/**", "**/*.pyc", "**/*.pyo"],
        "sbom": {"rust": False},
    }

    development = config["dependency-groups"]["dev"]
    assert set(development) == {
        "maturin==1.14.1",
        "mypy>=1.18.1",
        "PyYAML>=6.0.3",
        "pytest",
        "tomli>=1.1.0; python_version < '3.11'",
        "typing-extensions>=4.15.0",
        "wheel>=0.45.1",
    }

    locked_packages = {package["name"] for package in _toml(ROOT / "uv.lock")["package"]}
    assert {"pyyaml", "wheel"} <= locked_packages
    repository_wheels = subprocess.run(
        [
            "git",
            "ls-files",
            "--cached",
            "--others",
            "--exclude-standard",
            "--",
            "*.whl",
            ":(glob)**/*.whl",
        ],
        cwd=ROOT,
        check=True,
        capture_output=True,
        text=True,
    )
    assert repository_wheels.stdout == ""

    cache_files = {
        entry["file"]
        for entry in config["tool"]["uv"]["cache-keys"]
        if "file" in entry
    }
    assert {
        "pyproject.toml",
        "README.md",
        "rust/Cargo.toml",
        "rust/Cargo.lock",
        "rust/src/**/*.rs",
        "src/troupe/**/*.py",
        "src/troupe/**/*.pyi",
        "src/troupe/py.typed",
    } <= cache_files


def test_rust_manifest_and_step_one_source_boundary() -> None:
    config = _toml(ROOT / "rust" / "Cargo.toml")

    assert config["lib"]["name"] == "_runtime"
    assert config["lib"]["crate-type"] == ["cdylib"]

    dependencies = config["dependencies"]
    assert dependencies["pyo3"] == {
        "version": "0.29.0",
        "features": ["abi3-py310", "experimental-async"],
    }
    assert dependencies["pyo3-async-runtimes"] == {
        "version": "0.29.0",
        "features": ["tokio-runtime"],
    }
    assert dependencies["tokio"] == {
        "version": "1",
        "features": ["macros", "rt-multi-thread", "sync"],
    }
    assert dependencies["tokio-util"] == {
        "version": "0.7",
        "features": ["rt"],
    }
    assert dependencies["clap"] == {
        "version": "4",
        "features": ["derive"],
    }
    assert "extension-module" not in dependencies["pyo3"]["features"]

    rust_sources = sorted(
        path.relative_to(ROOT / "rust" / "src").as_posix()
        for path in (ROOT / "rust" / "src").rglob("*.rs")
    )
    assert {
        "cli.rs",
        "diagnostics.rs",
        "invocation.rs",
        "failure.rs",
        "lib.rs",
        "loader.rs",
        "production.rs",
        "python_task.rs",
        "runtime.rs",
        "signals.rs",
    } <= set(rust_sources)
    invocation_source = (ROOT / "rust" / "src" / "invocation.rs").read_text(
        encoding="utf-8"
    )
    assert "#[derive(Parser)]" in invocation_source
    assert "::try_parse_from" in invocation_source
    assert "#[pymodule(gil_used = true)]" in (
        ROOT / "rust" / "src" / "lib.rs"
    ).read_text(encoding="utf-8")
    for private_name in ("_Runtime", "PhaseFailure", "ProductionFailed"):
        for path in (
            ROOT / "src" / "troupe" / "__init__.py",
            ROOT / "src" / "troupe" / "__init__.pyi",
            ROOT / "README.md",
        ):
            assert private_name not in path.read_text(encoding="utf-8")


def test_installed_console_entry_point_targets_the_native_main() -> None:
    entries = [
        entry
        for entry in importlib.metadata.entry_points(group="console_scripts")
        if entry.name == "troupe"
    ]

    assert len(entries) == 1
    assert entries[0].value == "troupe._runtime:main"


def test_lockfiles_exist_and_are_nonempty() -> None:
    assert (ROOT / "uv.lock").stat().st_size > 0
    assert (ROOT / "rust" / "Cargo.lock").stat().st_size > 0


def test_verifier_accepts_the_exact_synthetic_layout(tmp_path: Path) -> None:
    verifier = _verifier()
    source, sdist, wheel = _synthetic_artifacts(tmp_path)

    verifier._validate_artifacts(source, sdist, wheel)


def test_verifier_accepts_pinned_maturin_entry_point_format(tmp_path: Path) -> None:
    verifier = _verifier()
    source, sdist, wheel = _synthetic_artifacts(tmp_path, entry_points="compact")

    verifier._validate_artifacts(source, sdist, wheel)


@pytest.mark.parametrize(
    "changes",
    [
        {"extra_source_python": True},
        {"extra_source_stub": True},
        {"extra_sdist_python": True},
        {"extra_sdist_stub": True},
        {"extra_wheel_python": True},
        {"extra_package_python": True},
        {"forbidden_file": "helper.py"},
        {"forbidden_file": "helper.pyi"},
        {"forbidden_file": "other_package/helper.pyi"},
        {"forbidden_file": "other_package/native.so"},
        {"source_stub": None},
        {"source_py_typed": False},
        {"sdist_stub": None},
        {"sdist_stub": b"wrong stub\n"},
        {"sdist_py_typed": False},
        {"wheel_stub": None},
        {"wheel_py_typed": False},
        {"source_wrapper": b"wrong wrapper\n"},
        {"sdist_wrapper": b"wrong wrapper\n"},
        {"wheel_wrapper": b"wrong wrapper\n"},
        {"wheel_stub": b"wrong stub\n"},
        {"native_count": 2},
        {"runtime_stem": "_runtime_helper"},
        {"extra_native": "_runtime.so"},
        {"extra_native": "other.so"},
        {"tag": "cp311-abi3-manylinux_2_17_x86_64"},
        {"tag": "cp310-abi3-linux_x86_64"},
        {"tag": "cp310-abi3-musllinux_1_2_x86_64"},
        {"tag": "cp310-abi3-manylinux_2_17_aarch64"},
        {"tag": "cp310-abi3-macosx_10_15_x86_64"},
        {"tag": "cp310-abi3-win_amd64"},
        {"tag": "cp313t-cp313t-manylinux_2_17_x86_64"},
        {"tag": "cp310-abi3-manylinux_2_17_x86_64.win_amd64"},
        {"tag": "cp310.cp311-abi3-manylinux_2_17_x86_64"},
        {"requires_dist": True},
        {"metadata_name": "other"},
        {"metadata_version": "9.9.9"},
        {"requires_python": ">=3.11"},
        {"wheel_version": None},
        {"wheel_version": "2.0"},
        {"root_is_purelib": "true"},
        {"entry_points": "missing"},
        {"entry_points": "wrong"},
        {"entry_points": "extra"},
        {"entry_points": "duplicate-key"},
        {"entry_points": "duplicate-section"},
        {"entry_points": "extra-section"},
        {"entry_points": "wrong-section"},
        {"entry_points": "default-section"},
        {"entry_points": "case-key"},
        {"entry_points": "colon-delimiter"},
        {"entry_points": "malformed"},
        {"entry_points": "invalid-utf8"},
        {
            "wheel_tags": ["cp310-abi3-manylinux2014_x86_64"],
        },
        {"wheel_tags": []},
        {
            "wheel_tags": [
                "cp310-abi3-manylinux_2_17_x86_64",
                "cp310-abi3-manylinux_2_17_x86_64",
            ],
        },
        {
            "tag": "cp310-abi3-manylinux_2_17_x86_64.manylinux2014_x86_64",
            "wheel_tags": ["cp310-abi3-manylinux_2_17_x86_64"],
        },
        {
            "wheel_tags": [
                "cp310-abi3-manylinux_2_17_x86_64",
                "cp310-abi3-manylinux2014_x86_64",
            ],
        },
        {"forbidden_file": "troupe/__pycache__/cached.pyc"},
        {"forbidden_file": "troupe/cache.pyc"},
        {"forbidden_file": "troupe/cache.pyo"},
        {"forbidden_file": "tests/test_runtime.py"},
        {"record_mutation": "wrong-size"},
        {"record_mutation": "wrong-hash"},
        {"record_mutation": "phantom"},
        {"record_mutation": "duplicate-row"},
        {"record_mutation": "missing-member-row"},
        {"record_mutation": "extra-column"},
        {"record_mutation": "invalid-size"},
        {"record_mutation": "negative-size"},
        {"record_mutation": "empty-hash"},
        {"record_mutation": "empty-size"},
        {"record_mutation": "missing-self-row"},
        {"record_mutation": "self-nonempty"},
        {"record_mutation": "missing-record"},
        {"forbidden_file": "/escape.dat"},
        {"forbidden_file": "../escape.dat"},
        {"sdist_unsafe": "traversal"},
        {"sdist_unsafe": "absolute"},
        {"sdist_unsafe": "symlink"},
        {"duplicate_member": True},
    ],
)
def test_verifier_rejects_invalid_artifacts(
    tmp_path: Path, changes: dict[str, object]
) -> None:
    verifier = _verifier()
    source, sdist, wheel = _synthetic_artifacts(tmp_path, **changes)

    with pytest.raises(verifier.VerificationError):
        verifier._validate_artifacts(source, sdist, wheel)


@pytest.mark.parametrize("reverse_wheel_tags", [False, True], ids=["forward", "reverse"])
def test_verifier_accepts_compressed_equivalent_manylinux_tags(
    tmp_path: Path,
    reverse_wheel_tags: bool,
) -> None:
    verifier = _verifier()
    tag = "cp310-abi3-manylinux_2_17_x86_64.manylinux2014_x86_64"
    wheel_tags = _expanded_tags(tag)
    if reverse_wheel_tags:
        wheel_tags.reverse()
    source, sdist, wheel = _synthetic_artifacts(
        tmp_path,
        tag=tag,
        wheel_tags=wheel_tags,
    )

    verifier._validate_artifacts(source, sdist, wheel)


@pytest.mark.parametrize(
    "tag",
    [
        "cp310-abi3-manylinux2014_x86_64",
        "cp310-abi3-manylinux_2_34_x86_64",
    ],
)
def test_verifier_accepts_supported_alias_and_newer_local_manylinux_tags(
    tmp_path: Path,
    tag: str,
) -> None:
    verifier = _verifier()
    source, sdist, wheel = _synthetic_artifacts(tmp_path, tag=tag)

    verifier._validate_artifacts(source, sdist, wheel)


def test_requested_manylinux_must_be_present_in_the_wheel_tag(tmp_path: Path) -> None:
    verifier = _verifier()
    source, _, newer = _synthetic_artifacts(
        tmp_path / "newer",
        tag="cp310-abi3-manylinux_2_34_x86_64",
    )
    with pytest.raises(verifier.VerificationError):
        verifier._validate_wheel(source, newer, required_manylinux="2_17")

    for name, tag in (
        ("numeric", "cp310-abi3-manylinux_2_17_x86_64"),
        ("alias", "cp310-abi3-manylinux2014_x86_64"),
    ):
        accepted_source, _, accepted = _synthetic_artifacts(
            tmp_path / name,
            tag=tag,
        )
        verifier._validate_wheel(
            accepted_source,
            accepted,
            required_manylinux="2_17",
        )


def test_verifier_build_command_and_environment_are_isolated(tmp_path: Path) -> None:
    verifier = _verifier()
    output = tmp_path / "artifacts"
    base = [
        "maturin",
        "build",
        "--sdist",
        "--locked",
        "--manifest-path",
        "rust/Cargo.toml",
        "--out",
        str(output),
    ]

    assert verifier._maturin_command(
        output,
        release=False,
        target=None,
        manylinux=None,
    ) == base
    assert verifier._maturin_command(
        output,
        release=True,
        target="x86_64-unknown-linux-gnu",
        manylinux="2_17",
    ) == [
        *base,
        "--release",
        "--target",
        "x86_64-unknown-linux-gnu",
        "--manylinux",
        "2_17",
    ]

    original = {
        "CONDA_PREFIX": "/conda",
        "PYTHONPATH": "/source",
        "PYTHONHOME": "/python",
        "VIRTUAL_ENV": "/outer-venv",
        "PYTHONDONTWRITEBYTECODE": "0",
        "PATH": os.environ.get("PATH", ""),
        "KEEP": "value",
    }
    build = verifier._build_environment(original)
    assert "CONDA_PREFIX" not in build
    assert build["PYTHONPATH"] == "/source"
    assert build["KEEP"] == "value"

    smoke = verifier._smoke_environment(original)
    assert {
        "CONDA_PREFIX",
        "PYTHONPATH",
        "PYTHONHOME",
        "VIRTUAL_ENV",
    }.isdisjoint(smoke)
    assert smoke["PYTHONDONTWRITEBYTECODE"] == "1"
    assert smoke["KEEP"] == "value"
    assert original["CONDA_PREFIX"] == "/conda"
    assert original["PYTHONDONTWRITEBYTECODE"] == "0"



def test_smoke_environment_injects_bytecode_guard_when_caller_omits_it() -> None:
    verifier = _verifier()
    original = {"KEEP": "value"}

    smoke = verifier._smoke_environment(original)

    assert smoke["PYTHONDONTWRITEBYTECODE"] == "1"
    assert smoke["KEEP"] == "value"
    assert "PYTHONDONTWRITEBYTECODE" not in original


def test_verifier_rejects_imports_outside_the_child_venv(tmp_path: Path) -> None:
    verifier = _verifier()
    child_venv = tmp_path / "child"
    installed = child_venv / "lib" / "python" / "site-packages" / "troupe"
    installed.mkdir(parents=True)
    wrapper = installed / "__init__.py"
    runtime = installed / "_runtime.abi3.so"
    dependency = installed.parent / "troupe_smoke_dependency.py"
    wrapper.touch()
    runtime.touch()
    dependency.touch()

    payload = {
        "troupe_file": str(wrapper),
        "runtime_file": str(runtime),
        "dependency_file": str(dependency),
    }

    verifier._validate_installed_paths(child_venv, payload)

    outside = tmp_path / "source" / "outside.bin"
    outside.parent.mkdir(parents=True)
    outside.touch()
    for key in ("troupe_file", "runtime_file", "dependency_file"):
        with pytest.raises(verifier.VerificationError):
            verifier._validate_installed_paths(
                child_venv,
                {**payload, key: str(outside)},
            )

        linked = installed.parent / f"linked-{key}"
        linked.symlink_to(outside)
        with pytest.raises(verifier.VerificationError):
            verifier._validate_installed_paths(
                child_venv,
                {**payload, key: str(linked)},
            )


def test_verifier_parser_accepts_only_the_two_exact_modes(tmp_path: Path) -> None:
    verifier = _verifier()
    parser = verifier._parser()
    output = tmp_path / "published"
    wheel = tmp_path / "troupe.whl"
    checksum = tmp_path / "SHA256SUMS"

    build = parser.parse_args(
        [
            "--build",
            "--release",
            "--target",
            "x86_64-unknown-linux-gnu",
            "--manylinux",
            "2_17",
            "--output-dir",
            str(output),
        ]
    )
    assert vars(build) == {
        "build": True,
        "wheel": None,
        "sha256_file": None,
        "release": True,
        "target": "x86_64-unknown-linux-gnu",
        "manylinux": "2_17",
        "output_dir": output,
    }

    supplied = parser.parse_args(
        ["--wheel", str(wheel), "--sha256-file", str(checksum)]
    )
    assert vars(supplied) == {
        "build": False,
        "wheel": wheel,
        "sha256_file": checksum,
        "release": False,
        "target": None,
        "manylinux": None,
        "output_dir": None,
    }


@pytest.mark.parametrize(
    "arguments",
    [
        [],
        ["--build", "--wheel", "wheel.whl", "--sha256-file", "SHA256SUMS"],
        ["--wheel", "wheel.whl"],
        ["--sha256-file", "SHA256SUMS"],
        ["--build", "--sha256-file", "SHA256SUMS"],
        ["--wheel", "wheel.whl", "--sha256-file", "SHA256SUMS", "--release"],
        [
            "--wheel",
            "wheel.whl",
            "--sha256-file",
            "SHA256SUMS",
            "--target",
            "x86_64-unknown-linux-gnu",
        ],
        [
            "--wheel",
            "wheel.whl",
            "--sha256-file",
            "SHA256SUMS",
            "--manylinux",
            "2_17",
        ],
        [
            "--wheel",
            "wheel.whl",
            "--sha256-file",
            "SHA256SUMS",
            "--output-dir",
            "out",
        ],
    ],
)
def test_verifier_parser_rejects_missing_mixed_or_mode_specific_arguments(
    arguments: list[str],
) -> None:
    verifier = _verifier()

    with pytest.raises(SystemExit) as raised:
        verifier._parser().parse_args(arguments)
    assert raised.value.code == 2


def test_sha256_file_is_strict_and_bound_to_the_exact_wheel(tmp_path: Path) -> None:
    verifier = _verifier()
    wheel = tmp_path / "troupe-0.1.0-cp310-abi3-manylinux_2_17_x86_64.whl"
    wheel.write_bytes(b"wheel bytes")
    checksum = tmp_path / "SHA256SUMS"
    digest = hashlib.sha256(wheel.read_bytes()).hexdigest()

    verifier._write_sha256(wheel, checksum)
    assert checksum.read_bytes() == f"{digest}  {wheel.name}\n".encode("ascii")
    verifier._validate_sha256(wheel, checksum)

    invalid = (
        f"{'0' * 64}  {wheel.name}\n",
        f"{digest}  other.whl\n",
        f"{digest.upper()}  {wheel.name}\n",
        f"{digest} {wheel.name}\n",
        f"{digest}\t{wheel.name}\n",
        f"{digest}  /tmp/{wheel.name}\n",
        f"{digest}  ./{wheel.name}\n",
        f"{digest}  {wheel.name}",
        f"{digest}  {wheel.name}\n\n",
        f"{digest}  {wheel.name}\n{digest}  {wheel.name}\n",
    )
    for index, content in enumerate(invalid):
        bad = tmp_path / f"bad-{index}"
        bad.write_text(content, encoding="ascii")
        with pytest.raises(verifier.VerificationError):
            verifier._validate_sha256(wheel, bad)

    non_ascii = tmp_path / "non-ascii"
    non_ascii.write_bytes(f"{digest}  ".encode("ascii") + b"\xff.whl\n")
    with pytest.raises(verifier.VerificationError):
        verifier._validate_sha256(wheel, non_ascii)


def _fake_build_run(
    events: list[str],
    *,
    expected_release: bool = False,
    expected_target: str | None = None,
    expected_manylinux: str | None = None,
):
    def run(
        command: list[str],
        *,
        cwd: Path,
        env: dict[str, str],
        **_: object,
    ) -> str:
        events.append("run-build")
        output = Path(command[command.index("--out") + 1])
        assert cwd == ROOT
        assert "CONDA_PREFIX" not in env
        verifier_command = [
            "maturin",
            "build",
            "--sdist",
            "--locked",
            "--manifest-path",
            "rust/Cargo.toml",
            "--out",
            str(output),
        ]
        if expected_release:
            verifier_command.append("--release")
        if expected_target is not None:
            verifier_command.extend(["--target", expected_target])
        if expected_manylinux is not None:
            verifier_command.extend(["--manylinux", expected_manylinux])
        assert command == verifier_command
        output.mkdir(parents=True, exist_ok=True)
        (output / "troupe-0.1.0.tar.gz").touch()
        (output / "troupe-0.1.0-cp310-abi3-manylinux_2_17_x86_64.whl").touch()
        return ""

    return run


def _patch_recording_validators(
    monkeypatch: pytest.MonkeyPatch,
    verifier: ModuleType,
    events: list[str],
    *,
    expected_manylinux: str | None = None,
) -> None:
    def source(*_: object, **__: object) -> tuple[bytes, bytes]:
        events.append("source")
        return EXPECTED_WRAPPER, EXPECTED_STUB

    def record(name: str):
        def inner(*_: object, **__: object) -> None:
            events.append(name)

        return inner

    monkeypatch.setattr(verifier, "_validate_source", source)
    monkeypatch.setattr(verifier, "_validate_sdist", record("sdist"))
    def wheel(
        *_: object,
        required_manylinux: str | None,
        **__: object,
    ) -> None:
        assert required_manylinux == expected_manylinux
        events.append("wheel")

    monkeypatch.setattr(verifier, "_validate_wheel", wheel)
    monkeypatch.setattr(verifier, "_smoke_wheel", record("smoke"))


def test_build_mode_records_one_build_then_all_evidence_before_publish(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    verifier = _verifier()
    output = tmp_path / "published"
    events: list[str] = []
    commits = 0

    monkeypatch.setenv("CONDA_PREFIX", "/outer-conda")
    monkeypatch.setattr(
        verifier,
        "_run",
        _fake_build_run(
            events,
            expected_release=True,
            expected_target="x86_64-unknown-linux-gnu",
            expected_manylinux="2_17",
        ),
    )
    _patch_recording_validators(
        monkeypatch,
        verifier,
        events,
        expected_manylinux="2_17",
    )

    def stage(wheel: Path, destination: Path) -> Path:
        assert wheel.suffix == ".whl"
        assert destination == output
        events.append("publish")
        staging = tmp_path / ".published-staging"
        staging.mkdir()
        (staging / wheel.name).touch()
        (staging / "SHA256SUMS").write_text("placeholder", encoding="ascii")
        return staging

    def commit(staging: Path, destination: Path) -> None:
        nonlocal commits
        commits += 1
        os.rename(staging, destination)

    monkeypatch.setattr(verifier, "_stage_publication", stage)
    monkeypatch.setattr(verifier, "_commit_publication", commit)
    monkeypatch.setattr(
        verifier,
        "_validate_sha256",
        lambda *_args, **_kwargs: pytest.fail("build mode must not consume a SHA file"),
    )

    assert verifier.main(
        [
            "--build",
            "--release",
            "--target",
            "x86_64-unknown-linux-gnu",
            "--manylinux",
            "2_17",
            "--output-dir",
            str(output),
        ]
    ) == 0
    assert events == ["run-build", "source", "sdist", "wheel", "smoke", "publish"]
    assert commits == 1
    assert output.is_dir()


def test_build_mode_never_publishes_when_smoke_fails(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    verifier = _verifier()
    output = tmp_path / "published"
    events: list[str] = []
    monkeypatch.setattr(verifier, "_run", _fake_build_run(events))
    _patch_recording_validators(monkeypatch, verifier, events)

    def fail_smoke(*_: object, **__: object) -> None:
        events.append("smoke")
        raise verifier.VerificationError("smoke failed")

    monkeypatch.setattr(verifier, "_smoke_wheel", fail_smoke)
    monkeypatch.setattr(
        verifier,
        "_stage_publication",
        lambda *_args, **_kwargs: pytest.fail("failed smoke must not publish"),
    )
    monkeypatch.setattr(
        verifier,
        "_commit_publication",
        lambda *_args, **_kwargs: pytest.fail("failed smoke must not commit"),
    )

    assert verifier.main(["--build", "--output-dir", str(output)]) == 1
    assert events == ["run-build", "source", "sdist", "wheel", "smoke"]
    assert not output.exists()


def test_supplied_wheel_mode_hashes_first_and_never_builds_or_reads_sdist(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    verifier = _verifier()
    wheel = tmp_path / "troupe.whl"
    checksum = tmp_path / "SHA256SUMS"
    wheel.touch()
    checksum.touch()
    events: list[str] = []

    monkeypatch.setattr(verifier, "_validate_sha256", lambda *_: events.append("hash"))
    _patch_recording_validators(monkeypatch, verifier, events)
    monkeypatch.setattr(
        verifier,
        "_validate_sdist",
        lambda *_args, **_kwargs: pytest.fail("wheel mode must not inspect an sdist"),
    )
    monkeypatch.setattr(
        verifier,
        "_run",
        lambda *_args, **_kwargs: pytest.fail("wheel mode must not run maturin"),
    )
    monkeypatch.setattr(
        verifier,
        "_stage_publication",
        lambda *_args, **_kwargs: pytest.fail("wheel mode must not publish"),
    )

    assert verifier.main(
        ["--wheel", str(wheel), "--sha256-file", str(checksum)]
    ) == 0
    assert events == ["hash", "source", "wheel", "smoke"]


def test_wrong_hash_short_circuits_every_other_boundary(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    verifier = _verifier()
    wheel = tmp_path / "troupe.whl"
    checksum = tmp_path / "SHA256SUMS"
    wheel.write_bytes(b"wheel")
    checksum.write_text(f"{'0' * 64}  {wheel.name}\n", encoding="ascii")
    calls = 0

    def forbidden(*_: object, **__: object) -> None:
        nonlocal calls
        calls += 1

    for name in (
        "_validate_source",
        "_validate_sdist",
        "_validate_wheel",
        "_smoke_wheel",
        "_run",
        "_stage_publication",
        "_commit_publication",
    ):
        monkeypatch.setattr(verifier, name, forbidden)

    assert verifier.main(
        ["--wheel", str(wheel), "--sha256-file", str(checksum)]
    ) == 1
    assert calls == 0


def test_output_directory_must_not_exist_before_build(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    verifier = _verifier()
    output = tmp_path / "published"
    output.mkdir()
    runs = 0

    def run(*_: object, **__: object) -> str:
        nonlocal runs
        runs += 1
        return ""

    monkeypatch.setattr(verifier, "_run", run)
    assert verifier.main(["--build", "--output-dir", str(output)]) == 1
    assert runs == 0


@pytest.mark.parametrize("pattern", ["*.whl", "*.tar.gz"])
@pytest.mark.parametrize("count", [0, 2])
def test_build_artifact_cardinality_must_be_exactly_one(
    tmp_path: Path,
    pattern: str,
    count: int,
) -> None:
    verifier = _verifier()
    suffix = ".whl" if pattern == "*.whl" else ".tar.gz"
    for index in range(count):
        (tmp_path / f"artifact-{index}{suffix}").touch()

    with pytest.raises(verifier.VerificationError):
        verifier._only_artifact(tmp_path, pattern)


@pytest.mark.parametrize(
    ("artifact", "count"),
    [("wheel", 0), ("wheel", 2), ("sdist", 0), ("sdist", 2)],
)
def test_build_main_rejects_wrong_artifact_cardinality_before_validation(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    artifact: str,
    count: int,
) -> None:
    verifier = _verifier()
    output = tmp_path / "published"

    def run(
        command: list[str],
        *,
        cwd: Path,
        env: dict[str, str],
        **_: object,
    ) -> str:
        assert cwd == ROOT
        assert "CONDA_PREFIX" not in env
        build_output = Path(command[command.index("--out") + 1])
        build_output.mkdir(parents=True)
        wheel_count = count if artifact == "wheel" else 1
        sdist_count = count if artifact == "sdist" else 1
        for index in range(wheel_count):
            (build_output / f"troupe-{index}-cp310-abi3-manylinux_2_17_x86_64.whl").touch()
        for index in range(sdist_count):
            (build_output / f"troupe-{index}.tar.gz").touch()
        return ""

    monkeypatch.setattr(verifier, "_run", run)
    for name in (
        "_validate_source",
        "_validate_sdist",
        "_validate_wheel",
        "_smoke_wheel",
        "_stage_publication",
    ):
        monkeypatch.setattr(
            verifier,
            name,
            lambda *_args, _name=name, **_kwargs: pytest.fail(
                f"{_name} must not run after invalid artifact cardinality"
            ),
        )

    assert verifier.main(["--build", "--output-dir", str(output)]) == 1
    assert not output.exists()


def test_publication_is_staged_then_atomically_renamed(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    verifier = _verifier()
    wheel = tmp_path / "troupe.whl"
    wheel.write_bytes(b"wheel")
    output = tmp_path / "wheel-artifact"
    parent = output.parent.stat()

    def forbidden_stage_metadata(*_: object) -> None:
        pytest.fail("publication metadata must not change while staging")

    with monkeypatch.context() as stage_metadata:
        stage_metadata.setattr(verifier.Path, "chmod", forbidden_stage_metadata)
        stage_metadata.setattr(verifier.os, "chown", forbidden_stage_metadata)
        staging = verifier._stage_publication(wheel, output)
    staged_wheel = staging / wheel.name
    checksum = staging / "SHA256SUMS"
    assert staging.parent == output.parent
    assert staging != output
    assert not output.exists()
    assert sorted(path.name for path in staging.iterdir()) == ["SHA256SUMS", wheel.name]
    assert staging.stat().st_mode & 0o777 == 0o700
    assert staging.stat().st_uid == os.geteuid()

    events: list[str] = []
    real_validate_sha256 = verifier._validate_sha256

    def validate_sha256(staged: Path, recorded_checksum: Path) -> None:
        events.append("recheck")
        real_validate_sha256(staged, recorded_checksum)

    chmod_calls: list[tuple[Path, int]] = []
    real_chmod = verifier.Path.chmod

    def chmod(path: Path, mode: int) -> None:
        chmod_calls.append((path, mode))
        events.append(f"chmod:{path.name}")
        real_chmod(path, mode)

    chown_calls: list[tuple[Path, int, int]] = []
    real_chown = verifier.os.chown

    def chown(path: Path, uid: int, gid: int) -> None:
        resolved = Path(path)
        chown_calls.append((resolved, uid, gid))
        events.append(f"chown:{resolved.name}")
        real_chown(path, uid, gid)

    rename_calls: list[tuple[Path, Path]] = []
    real_rename = verifier.os.rename

    def rename(source: Path, destination: Path) -> None:
        assert events == [
            "recheck",
            f"chmod:{wheel.name}",
            "chmod:SHA256SUMS",
            f"chmod:{staging.name}",
            f"chown:{wheel.name}",
            "chown:SHA256SUMS",
            f"chown:{staging.name}",
        ]
        assert stat.S_IMODE(staging.stat().st_mode) == 0o755
        assert stat.S_IMODE(staged_wheel.stat().st_mode) == 0o644
        assert stat.S_IMODE(checksum.stat().st_mode) == 0o644
        assert {
            (path.stat().st_uid, path.stat().st_gid)
            for path in (staging, staged_wheel, checksum)
        } == {(parent.st_uid, parent.st_gid)}
        events.append("rename")
        rename_calls.append((source, destination))
        real_rename(source, destination)

    monkeypatch.setattr(verifier, "_validate_sha256", validate_sha256)
    monkeypatch.setattr(verifier.Path, "chmod", chmod)
    monkeypatch.setattr(verifier.os, "chown", chown)
    monkeypatch.setattr(verifier.os, "rename", rename)

    verifier._commit_publication(staging, output)
    assert set(chmod_calls) == {
        (staging, 0o755),
        (staged_wheel, 0o644),
        (checksum, 0o644),
    }
    assert set(chown_calls) == {
        (staging, parent.st_uid, parent.st_gid),
        (staged_wheel, parent.st_uid, parent.st_gid),
        (checksum, parent.st_uid, parent.st_gid),
    }
    assert rename_calls == [(staging, output)]
    assert events[-1] == "rename"
    assert output.is_dir()
    assert not staging.exists()
    assert sorted(path.name for path in output.iterdir()) == ["SHA256SUMS", wheel.name]
    real_validate_sha256(output / wheel.name, output / "SHA256SUMS")
    assert stat.S_IMODE(output.stat().st_mode) == 0o755
    assert stat.S_IMODE((output / wheel.name).stat().st_mode) == 0o644
    assert stat.S_IMODE((output / "SHA256SUMS").stat().st_mode) == 0o644
    assert (output.stat().st_uid, output.stat().st_gid) == (
        parent.st_uid,
        parent.st_gid,
    )


def test_publication_owner_comes_from_output_parent(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    verifier = _verifier()
    wheel = tmp_path / "troupe.whl"
    wheel.write_bytes(b"wheel")
    output = tmp_path / "wheel-artifact"
    staging = verifier._stage_publication(wheel, output)
    sentinel_uid = os.geteuid() + 10_000
    sentinel_gid = os.getegid() + 20_000
    real_stat = verifier.Path.stat

    class ParentStat:
        st_uid = sentinel_uid
        st_gid = sentinel_gid

    def path_stat(path: Path, *args: object, **kwargs: object) -> object:
        if path == output.parent:
            return ParentStat()
        return real_stat(path, *args, **kwargs)

    chown_calls: list[tuple[Path, int, int]] = []

    def chown(path: Path, uid: int, gid: int) -> None:
        chown_calls.append((Path(path), uid, gid))

    monkeypatch.setattr(verifier.Path, "stat", path_stat)
    monkeypatch.setattr(verifier.os, "chown", chown)

    verifier._commit_publication(staging, output)

    assert chown_calls == [
        (staging / wheel.name, sentinel_uid, sentinel_gid),
        (staging / "SHA256SUMS", sentinel_uid, sentinel_gid),
        (staging, sentinel_uid, sentinel_gid),
    ]
    assert output.is_dir()


def test_publication_failure_cleans_staging_and_leaves_output_absent(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    verifier = _verifier()
    wheel = tmp_path / "troupe.whl"
    wheel.write_bytes(b"wheel")
    output = tmp_path / "wheel-artifact"

    monkeypatch.setattr(shutil, "copy2", lambda *_: (_ for _ in ()).throw(OSError("copy")))
    with pytest.raises(verifier.VerificationError):
        verifier._stage_publication(wheel, output)
    assert not output.exists()
    assert not list(tmp_path.glob(".wheel-artifact-*"))


@pytest.mark.parametrize("operation", ["mode", "owner"])
@pytest.mark.parametrize("target_name", ["directory", "wheel", "checksum"])
def test_publication_metadata_failure_cleans_staging(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    operation: str,
    target_name: str,
) -> None:
    verifier = _verifier()
    wheel = tmp_path / "troupe.whl"
    wheel.write_bytes(b"wheel")
    output = tmp_path / "wheel-artifact"
    staging = verifier._stage_publication(wheel, output)
    targets = {
        "directory": staging,
        "wheel": staging / wheel.name,
        "checksum": staging / "SHA256SUMS",
    }
    target = targets[target_name]

    if operation == "mode":
        real_chmod = verifier.Path.chmod

        def chmod(path: Path, mode: int) -> None:
            if path == target:
                raise OSError(f"{operation} failure on {target_name}")
            real_chmod(path, mode)

        monkeypatch.setattr(verifier.Path, "chmod", chmod)
    else:
        real_chown = verifier.os.chown

        def chown(path: Path, uid: int, gid: int) -> None:
            if Path(path) == target:
                raise OSError(f"{operation} failure on {target_name}")
            real_chown(path, uid, gid)

        monkeypatch.setattr(verifier.os, "chown", chown)

    monkeypatch.setattr(
        verifier.os,
        "rename",
        lambda *_: pytest.fail("rename must not run after publication metadata failure"),
    )

    with pytest.raises(verifier.VerificationError):
        verifier._commit_publication(staging, output)
    assert not staging.exists()
    assert not output.exists()
    assert not list(tmp_path.glob(".wheel-artifact-*"))


def test_commit_checksum_failure_precedes_metadata_and_rename(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    verifier = _verifier()
    wheel = tmp_path / "troupe.whl"
    wheel.write_bytes(b"wheel")
    output = tmp_path / "wheel-artifact"
    staging = verifier._stage_publication(wheel, output)

    def fail_recheck(*_: object) -> None:
        raise verifier.VerificationError("checksum recheck")

    def forbidden(*_: object) -> None:
        pytest.fail("metadata and rename must not run after checksum recheck failure")

    monkeypatch.setattr(verifier, "_validate_sha256", fail_recheck)
    monkeypatch.setattr(verifier.Path, "chmod", forbidden)
    monkeypatch.setattr(verifier.os, "chown", forbidden)
    monkeypatch.setattr(verifier.os, "rename", forbidden)

    with pytest.raises(verifier.VerificationError):
        verifier._commit_publication(staging, output)
    assert not staging.exists()
    assert not output.exists()
    assert not list(tmp_path.glob(".wheel-artifact-*"))


@pytest.mark.parametrize("boundary", ["write", "recheck"])
def test_checksum_failure_cleans_staging_and_leaves_output_absent(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    boundary: str,
) -> None:
    verifier = _verifier()
    wheel = tmp_path / "troupe.whl"
    wheel.write_bytes(b"wheel")
    output = tmp_path / "wheel-artifact"

    if boundary == "write":
        monkeypatch.setattr(
            verifier,
            "_write_sha256",
            lambda *_: (_ for _ in ()).throw(OSError("checksum write")),
        )
    else:
        monkeypatch.setattr(
            verifier,
            "_validate_sha256",
            lambda *_: (_ for _ in ()).throw(
                verifier.VerificationError("checksum recheck")
            ),
        )

    with pytest.raises(verifier.VerificationError):
        verifier._stage_publication(wheel, output)
    assert not output.exists()
    assert not list(tmp_path.glob(".wheel-artifact-*"))


def test_rename_failure_cleans_staging_and_leaves_output_absent(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    verifier = _verifier()
    wheel = tmp_path / "troupe.whl"
    wheel.write_bytes(b"wheel")
    output = tmp_path / "wheel-artifact"
    staging = verifier._stage_publication(wheel, output)
    monkeypatch.setattr(
        verifier.os,
        "rename",
        lambda *_: (_ for _ in ()).throw(OSError("rename")),
    )

    with pytest.raises(verifier.VerificationError):
        verifier._commit_publication(staging, output)
    assert not staging.exists()
    assert not output.exists()


def test_build_cleanup_finishes_before_atomic_commit(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    verifier = _verifier()
    output = tmp_path / "published"
    build_workspace = tmp_path / "build-workspace"
    events: list[str] = []

    class RecordingTemporaryDirectory:
        def __init__(self, **_: object) -> None:
            pass

        def __enter__(self) -> str:
            build_workspace.mkdir()
            return str(build_workspace)

        def __exit__(self, *_: object) -> None:
            shutil.rmtree(build_workspace)
            events.append("cleanup")

    monkeypatch.setattr(verifier.tempfile, "TemporaryDirectory", RecordingTemporaryDirectory)
    monkeypatch.setattr(verifier, "_run", _fake_build_run(events))
    _patch_recording_validators(monkeypatch, verifier, events)

    def stage(wheel: Path, destination: Path) -> Path:
        events.append("publish")
        staging = tmp_path / ".published-staging"
        staging.mkdir()
        (staging / wheel.name).touch()
        return staging

    def commit(staging: Path, destination: Path) -> None:
        events.append("commit")
        os.rename(staging, destination)

    monkeypatch.setattr(verifier, "_stage_publication", stage)
    monkeypatch.setattr(verifier, "_commit_publication", commit)

    assert verifier.main(["--build", "--output-dir", str(output)]) == 0
    assert events.index("publish") < events.index("cleanup") < events.index("commit")


def test_build_cleanup_failure_never_exposes_final_output(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    verifier = _verifier()
    output = tmp_path / "published"
    build_workspace = tmp_path / "build-workspace"
    staged: list[Path] = []
    events: list[str] = []

    class FailingTemporaryDirectory:
        def __init__(self, **_: object) -> None:
            pass

        def __enter__(self) -> str:
            build_workspace.mkdir()
            return str(build_workspace)

        def __exit__(self, *_: object) -> None:
            shutil.rmtree(build_workspace)
            raise OSError("temporary cleanup failed")

    monkeypatch.setattr(verifier.tempfile, "TemporaryDirectory", FailingTemporaryDirectory)
    monkeypatch.setattr(verifier, "_run", _fake_build_run(events))
    _patch_recording_validators(monkeypatch, verifier, events)
    real_stage = verifier._stage_publication

    def recording_stage(wheel: Path, destination: Path) -> Path:
        result = real_stage(wheel, destination)
        staged.append(result)
        return result

    monkeypatch.setattr(verifier, "_stage_publication", recording_stage)

    assert verifier.main(["--build", "--output-dir", str(output)]) == 1
    assert not output.exists()
    assert staged and all(not path.exists() for path in staged)


def test_dependency_wheel_is_generated_with_valid_metadata_and_record(
    tmp_path: Path,
) -> None:
    verifier = _verifier()

    wheel = verifier._build_dependency_wheel(tmp_path)
    assert wheel.name == "troupe_smoke_dependency-1.0.0-py3-none-any.whl"
    with WheelFile(wheel) as archive:
        names = sorted(name for name in archive.namelist() if not name.endswith("/"))
        for name in names:
            archive.read(name)
        assert names == [
            "troupe_smoke_dependency-1.0.0.dist-info/METADATA",
            "troupe_smoke_dependency-1.0.0.dist-info/RECORD",
            "troupe_smoke_dependency-1.0.0.dist-info/WHEEL",
            "troupe_smoke_dependency.py",
        ]
        assert archive.read("troupe_smoke_dependency.py") == (
            ROOT / "tests" / "fixtures" / "wheel_smoke_dependency.py"
        ).read_bytes()
        metadata = archive.read(
            "troupe_smoke_dependency-1.0.0.dist-info/METADATA"
        ).decode("utf-8")
        assert "Name: troupe-smoke-dependency\n" in metadata
        assert "Version: 1.0.0\n" in metadata
        assert "Requires-Dist:" not in metadata
        wheel_metadata = archive.read(
            "troupe_smoke_dependency-1.0.0.dist-info/WHEEL"
        ).decode("utf-8")
        assert "Wheel-Version: 1.0\n" in wheel_metadata
        assert "Root-Is-Purelib: true\n" in wheel_metadata
        assert "Tag: py3-none-any\n" in wheel_metadata

    with zipfile.ZipFile(wheel) as archive:
        infos = [info for info in archive.infolist() if not info.is_dir()]
        assert len({info.filename for info in infos}) == len(infos)
        record_name = "troupe_smoke_dependency-1.0.0.dist-info/RECORD"
        rows = list(
            csv.reader(io.StringIO(archive.read(record_name).decode("utf-8")))
        )
        assert all(len(row) == 3 for row in rows)
        assert len({row[0] for row in rows}) == len(rows)
        assert {row[0] for row in rows} == {info.filename for info in infos}
        for path, encoded_hash, size in rows:
            if path == record_name:
                assert (encoded_hash, size) == ("", "")
                continue
            data = archive.read(path)
            digest = base64.urlsafe_b64encode(hashlib.sha256(data).digest()).rstrip(b"=")
            assert encoded_hash == f"sha256={digest.decode('ascii')}"
            assert size == str(len(data))


def _installed_smoke_payload(child: Path) -> dict[str, object]:
    installed = child / "lib" / "python" / "site-packages"
    return {
        "troupe_file": str(installed / "troupe" / "__init__.py"),
        "runtime_file": str(installed / "troupe" / "_runtime.abi3.so"),
        "dependency_file": str(installed / "troupe_smoke_dependency.py"),
        "production_identity": True,
        "production_module": "troupe",
        "exports": ["Production"],
        "gil_disabled": False,
        "surrogate_constructor": True,
        "default_hooks": True,
        "subclass_override": True,
        "entry_points": [["troupe", "troupe._runtime:main"]],
    }


@pytest.mark.parametrize(
    ("key", "value"),
    [
        ("production_identity", False),
        ("production_module", "troupe._runtime"),
        ("exports", []),
        ("exports", ["Production", "Other"]),
        ("exports", ["Other"]),
        ("gil_disabled", True),
        ("surrogate_constructor", False),
        ("default_hooks", False),
        ("subclass_override", False),
        ("entry_points", []),
        ("entry_points", [["troupe", "troupe:main"]]),
        (
            "entry_points",
            [
                ["troupe", "troupe._runtime:main"],
                ["extra", "troupe._runtime:main"],
            ],
        ),
    ],
)
def test_smoke_payload_rejects_every_wrong_api_or_interpreter_fact(
    tmp_path: Path,
    key: str,
    value: object,
) -> None:
    verifier = _verifier()
    child = tmp_path / "child"
    payload = _installed_smoke_payload(child)
    for path_key in ("troupe_file", "runtime_file", "dependency_file"):
        Path(str(payload[path_key])).parent.mkdir(parents=True, exist_ok=True)
        Path(str(payload[path_key])).touch()
    payload[key] = value

    with pytest.raises(verifier.VerificationError):
        verifier._validate_smoke_payload(child, payload)


@pytest.mark.parametrize("missing", list(_installed_smoke_payload(Path("/child"))))
def test_smoke_payload_requires_every_field(tmp_path: Path, missing: str) -> None:
    verifier = _verifier()
    child = tmp_path / "child"
    payload = _installed_smoke_payload(child)
    for path_key in ("troupe_file", "runtime_file", "dependency_file"):
        installed_file = Path(str(payload[path_key]))
        installed_file.parent.mkdir(parents=True, exist_ok=True)
        installed_file.touch()
    payload.pop(missing)

    with pytest.raises(verifier.VerificationError):
        verifier._validate_smoke_payload(child, payload)


def test_smoke_payload_consumes_the_installed_path_validator(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    verifier = _verifier()
    child = tmp_path / "child"
    payload = _installed_smoke_payload(child)
    calls: list[tuple[Path, dict[str, object]]] = []

    def paths(root: Path, received: dict[str, object]) -> None:
        calls.append((root, received))

    monkeypatch.setattr(verifier, "_validate_installed_paths", paths)
    verifier._validate_smoke_payload(child, payload)
    assert calls == [(child, payload)]


def _execute_child_probe(
    verifier: ModuleType,
    monkeypatch: pytest.MonkeyPatch,
    mutation: str | None,
) -> dict[str, object]:
    class ProbeAwaitable:
        def __init__(self, result: object = None, error: Exception | None = None) -> None:
            self.result = result
            self.error = error

        def __await__(self) -> Generator[None, None, object]:
            if False:
                yield None
            if self.error is not None:
                raise self.error
            return self.result

    class ProbeProduction:
        def __init_subclass__(cls, **kwargs: object) -> None:
            if mutation == "subclass":
                raise RuntimeError("subclass disabled")
            super().__init_subclass__(**kwargs)

        def __init__(self, *positional: object, **keywords: object) -> None:
            if mutation == "constructor":
                return
            if keywords or len(positional) != 1:
                raise TypeError("args must be positional-only")
            args = positional[0]
            if type(args) is not list or any(type(arg) is not str for arg in args):
                raise TypeError("args must be list[str]")
            if mutation == "surrogate" and args == ["\udcff"]:
                raise TypeError("surrogate rejected")

        def start(self) -> ProbeAwaitable:
            return ProbeAwaitable("wrong" if mutation == "start" else None)

        def scene(self) -> ProbeAwaitable:
            if mutation == "scene":
                return ProbeAwaitable()
            return ProbeAwaitable(
                error=NotImplementedError("Production.scene() is not implemented")
            )

        def stop(self) -> ProbeAwaitable:
            return ProbeAwaitable("wrong" if mutation == "stop" else None)

    ProbeProduction.__module__ = "wrong" if mutation == "module" else "troupe"

    package = ModuleType("troupe")
    package.__path__ = []
    package.__file__ = "/child/lib/python/site-packages/troupe/__init__.py"
    package.Production = ProbeProduction
    package.__all__ = ["Other"] if mutation == "exports" else ["Production"]
    runtime = ModuleType("troupe._runtime")
    runtime.__file__ = "/child/lib/python/site-packages/troupe/_runtime.abi3.so"
    if mutation == "identity":
        class OtherProduction(ProbeProduction):
            pass

        OtherProduction.__module__ = "troupe"
        runtime.Production = OtherProduction
    else:
        runtime.Production = ProbeProduction
    package._runtime = runtime
    dependency = ModuleType("troupe_smoke_dependency")
    dependency.__file__ = "/child/lib/python/site-packages/troupe_smoke_dependency.py"
    dependency.VALUE = "dependency-ok"
    monkeypatch.setitem(sys.modules, "troupe", package)
    monkeypatch.setitem(sys.modules, "troupe._runtime", runtime)
    monkeypatch.setitem(sys.modules, "troupe_smoke_dependency", dependency)

    class EntryPoint:
        name = "troupe"
        value = "troupe:main" if mutation == "entrypoint" else "troupe._runtime:main"

    monkeypatch.setattr(
        importlib.metadata,
        "entry_points",
        lambda *, group: [EntryPoint()] if group == "console_scripts" else [],
    )
    real_config_var = sysconfig.get_config_var
    monkeypatch.setattr(
        sysconfig,
        "get_config_var",
        lambda name: 1 if mutation == "gil" and name == "Py_GIL_DISABLED" else real_config_var(name),
    )

    output = io.StringIO()
    with contextlib.redirect_stdout(output):
        exec(compile(verifier.SMOKE, "<troupe-wheel-smoke>", "exec"), {})
    return json.loads(output.getvalue())


def test_child_probe_executes_and_reports_every_required_check(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    verifier = _verifier()
    payload = _execute_child_probe(verifier, monkeypatch, None)
    expected = _installed_smoke_payload(Path("/child"))
    assert payload == expected


@pytest.mark.parametrize(
    ("mutation", "error_type"),
    [
        ("identity", AssertionError),
        ("module", AssertionError),
        ("exports", AssertionError),
        ("gil", AssertionError),
        ("constructor", AssertionError),
        ("surrogate", TypeError),
        ("start", AssertionError),
        ("scene", AssertionError),
        ("stop", AssertionError),
        ("subclass", RuntimeError),
        ("entrypoint", AssertionError),
    ],
)
def test_child_probe_fails_when_each_runtime_fact_is_wrong(
    monkeypatch: pytest.MonkeyPatch,
    mutation: str,
    error_type: type[Exception],
) -> None:
    verifier = _verifier()
    with pytest.raises(error_type):
        _execute_child_probe(verifier, monkeypatch, mutation)


@pytest.mark.parametrize(
    "events",
    [
        [],
        [["args", []], ["start"], ["scene", "dependency-ok", "module-ok", "resource-ok"], ["stop"]],
        [["args", ["wrong"]], ["start"], ["scene", "dependency-ok", "module-ok", "resource-ok"], ["stop"]],
        [["args", []], ["scene", "dependency-ok", "module-ok", "resource-ok"], ["stop"]],
        [["args", []], ["start"], ["scene", "wrong", "module-ok", "resource-ok"], ["stop"]],
        [["args", []], ["start"], ["scene", "dependency-ok", "module-ok", "resource-ok"], ["stop"], ["extra"]],
    ],
)
def test_smoke_event_log_is_compared_as_one_exact_json_value(
    tmp_path: Path,
    events: list[list[object]],
) -> None:
    verifier = _verifier()
    path = tmp_path / "events.json"
    raw_args = ["--events", str(path), "--value", "7", "input.txt"]
    expected = [
        ["args", raw_args],
        ["start"],
        ["scene", "dependency-ok", "module-ok", "resource-ok"],
        ["stop"],
    ]
    path.write_text(json.dumps(expected), encoding="utf-8")
    verifier._validate_smoke_events(path, raw_args)

    path.write_text(json.dumps(events), encoding="utf-8")
    with pytest.raises(verifier.VerificationError):
        verifier._validate_smoke_events(path, raw_args)


def test_smoke_event_log_rejects_missing_or_invalid_json(tmp_path: Path) -> None:
    verifier = _verifier()
    path = tmp_path / "events.json"
    with pytest.raises(verifier.VerificationError):
        verifier._validate_smoke_events(path, [])
    path.write_text("not json", encoding="utf-8")
    with pytest.raises(verifier.VerificationError):
        verifier._validate_smoke_events(path, [])


@pytest.mark.parametrize(
    ("uv_found", "wrong_troupe"),
    [
        (True, False),
        (False, True),
    ],
)
def test_smoke_tool_resolution_rejects_uv_or_the_wrong_console(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    uv_found: bool,
    wrong_troupe: bool,
) -> None:
    verifier = _verifier()
    child = tmp_path / "child"
    expected_troupe = str(child / "bin" / "troupe")

    def which(name: str, *, path: str) -> str | None:
        assert path == f"{child}/bin:/usr/bin:/bin"
        if name == "uv":
            return "/usr/bin/uv" if uv_found else None
        return "/other/bin/troupe" if wrong_troupe else expected_troupe

    monkeypatch.setattr(verifier.shutil, "which", which)
    with pytest.raises(verifier.VerificationError):
        verifier._validate_smoke_tools(child, {"PATH": f"{child}/bin:/usr/bin:/bin"})


def test_run_rejects_a_forbidden_stderr_marker_even_on_zero_exit(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    verifier = _verifier()

    class Completed:
        returncode = 0
        stdout = ""
        stderr = "troupe: failed to run production\n"

    monkeypatch.setattr(verifier.subprocess, "run", lambda *_, **__: Completed())
    with pytest.raises(verifier.VerificationError):
        verifier._run(
            ["troupe", "--help"],
            cwd=tmp_path,
            env={},
            forbidden_stderr="troupe:",
        )


def test_clean_smoke_wiring_uses_child_python_offline_and_literal_console(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    verifier = _verifier()
    wheel = tmp_path / "troupe.whl"
    wheel.touch()
    workspace = tmp_path / "smoke-workspace"
    workspace.mkdir()
    child = workspace / "child-venv"
    outside = workspace / "outside-repository"
    events_path = workspace / "events.json"
    fixture = ROOT / "tests" / "fixtures" / "productions" / "wheel_smoke_production"
    raw_args = ["--events", str(events_path), "--value", "7", "input.txt"]
    expected_events = [
        ["args", raw_args],
        ["start"],
        ["scene", "dependency-ok", "module-ok", "resource-ok"],
        ["stop"],
    ]
    managed_python = tmp_path / "managed" / "bin" / "python3.10"
    managed_python.parent.mkdir(parents=True)
    managed_python.touch()
    project_python = tmp_path / "project" / "bin" / "python"
    project_python.parent.mkdir(parents=True)
    project_python.symlink_to(managed_python)
    original_base_executable = str(project_python)
    resolved_base_executable = str(managed_python.resolve())
    monkeypatch.setattr(verifier.sys, "_base_executable", original_base_executable)
    calls: list[tuple[list[str], Path, dict[str, str], dict[str, object]]] = []
    which_calls: list[tuple[str, str]] = []
    timeline: list[str] = []
    builder_flags: list[bool] = []
    builder_init_base_executables: list[str] = []
    builder_base_executables: list[str] = []
    post_create_base_executables: list[str] = []

    class FakeEnvBuilder:
        def __init__(self, *, with_pip: bool) -> None:
            builder_flags.append(with_pip)
            builder_init_base_executables.append(verifier.sys._base_executable)

        def create(self, path: Path) -> None:
            builder_base_executables.append(verifier.sys._base_executable)
            bin_dir = path / "bin"
            bin_dir.mkdir(parents=True)
            for name in ("python", "troupe"):
                executable = bin_dir / name
                executable.write_text("#!/bin/sh\nexit 0\n", encoding="utf-8")
                executable.chmod(0o755)
            payload = _installed_smoke_payload(path)
            for key in ("troupe_file", "runtime_file", "dependency_file"):
                installed_file = Path(str(payload[key]))
                installed_file.parent.mkdir(parents=True, exist_ok=True)
                installed_file.touch()

    monkeypatch.setattr(verifier.venv, "EnvBuilder", FakeEnvBuilder)
    dependency = workspace / "dependency.whl"
    dependency.touch()

    def build_dependency(_: Path) -> Path:
        post_create_base_executables.append(verifier.sys._base_executable)
        return dependency

    monkeypatch.setattr(verifier, "_build_dependency_wheel", build_dependency)

    child_python = str(child / "bin" / "python")
    expected_commands = [
        [
            child_python,
            "-m",
            "pip",
            "install",
            "--no-index",
            "--no-deps",
            str(wheel.resolve()),
            str(dependency.resolve()),
        ],
        [child_python, "-m", "pip", "check"],
        [child_python, "-c", verifier.SMOKE],
        ["troupe", "--help"],
        ["troupe", "--production", str(fixture), "--", *raw_args],
    ]

    def run(
        command: list[str],
        *,
        cwd: Path,
        env: dict[str, str],
        **kwargs: object,
    ) -> str:
        index = len(calls)
        assert command == expected_commands[index]
        timeline.append(f"run-{index}")
        calls.append((command, cwd, dict(env), dict(kwargs)))
        if index == 2:
            return json.dumps(_installed_smoke_payload(child))
        if index == 4:
            events_path.write_text(json.dumps(expected_events), encoding="utf-8")
        return ""

    monkeypatch.setattr(verifier, "_run", run)
    real_which = shutil.which

    def which(name: str, *, path: str) -> str | None:
        timeline.append(f"which-{name}")
        which_calls.append((name, path))
        return real_which(name, path=path)

    monkeypatch.setattr(verifier.shutil, "which", which)
    validations = {"tools": 0, "payload": 0, "paths": 0, "events": 0}
    for name, key in (
        ("_validate_smoke_tools", "tools"),
        ("_validate_smoke_payload", "payload"),
        ("_validate_installed_paths", "paths"),
        ("_validate_smoke_events", "events"),
    ):
        real = getattr(verifier, name)

        def recording(*args: object, _real=real, _key=key, **kwargs: object):
            timeline.append(f"validate-{_key}")
            validations[_key] += 1
            return _real(*args, **kwargs)

        monkeypatch.setattr(verifier, name, recording)

    for name in ("CONDA_PREFIX", "PYTHONPATH", "PYTHONHOME", "VIRTUAL_ENV"):
        monkeypatch.setenv(name, f"/{name.lower()}")
    monkeypatch.delenv("PYTHONDONTWRITEBYTECODE", raising=False)

    verifier._smoke_wheel(wheel, workspace)

    assert builder_flags == [True]
    assert builder_init_base_executables == [original_base_executable]
    assert builder_base_executables == [resolved_base_executable]
    assert post_create_base_executables == [original_base_executable]
    assert verifier.sys._base_executable == original_base_executable
    assert len(calls) == 5
    expected_path = f"{child}/bin:/usr/bin:/bin"
    assert which_calls == [("uv", expected_path), ("troupe", expected_path)]
    assert validations == {"tools": 1, "payload": 1, "paths": 1, "events": 1}
    assert timeline == [
        "run-0",
        "run-1",
        "run-2",
        "validate-payload",
        "validate-paths",
        "validate-tools",
        "which-uv",
        "which-troupe",
        "run-3",
        "run-4",
        "validate-events",
    ]
    for index, (_, cwd, env, kwargs) in enumerate(calls):
        assert cwd == outside
        assert env["PATH"] == expected_path
        assert {
            "CONDA_PREFIX",
            "PYTHONPATH",
            "PYTHONHOME",
            "VIRTUAL_ENV",
        }.isdisjoint(env)
        assert env["PYTHONDONTWRITEBYTECODE"] == "1"
        assert kwargs == ({"forbidden_stderr": "troupe:"} if index in (3, 4) else {})


@pytest.mark.parametrize("failure", ["os-error", "called-process-error", "abort"])
def test_clean_smoke_restores_base_executable_when_venv_creation_fails(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    failure: str,
) -> None:
    verifier = _verifier()
    wheel = tmp_path / "troupe.whl"
    wheel.touch()
    workspace = tmp_path / "smoke-workspace"
    workspace.mkdir()
    managed_python = tmp_path / "managed" / "bin" / "python3.10"
    managed_python.parent.mkdir(parents=True)
    managed_python.touch()
    project_python = tmp_path / "project" / "bin" / "python"
    project_python.parent.mkdir(parents=True)
    project_python.symlink_to(managed_python)
    original_base_executable = str(project_python)
    resolved_base_executable = str(managed_python.resolve())
    monkeypatch.setattr(verifier.sys, "_base_executable", original_base_executable)
    builder_init_base_executables: list[str] = []
    builder_base_executables: list[str] = []

    class BuilderAbort(BaseException):
        pass

    errors: dict[str, BaseException] = {
        "os-error": OSError("venv creation failed"),
        "called-process-error": subprocess.CalledProcessError(1, ["ensurepip"]),
        "abort": BuilderAbort("venv creation aborted"),
    }
    builder_error = errors[failure]

    class FailingEnvBuilder:
        def __init__(self, *, with_pip: bool) -> None:
            assert with_pip is True
            builder_init_base_executables.append(verifier.sys._base_executable)

        def create(self, _: Path) -> None:
            builder_base_executables.append(verifier.sys._base_executable)
            raise builder_error

    monkeypatch.setattr(verifier.venv, "EnvBuilder", FailingEnvBuilder)

    if failure == "os-error":
        with pytest.raises(verifier.VerificationError, match="could not create child venv"):
            verifier._smoke_wheel(wheel, workspace)
    else:
        with pytest.raises(type(builder_error)) as caught:
            verifier._smoke_wheel(wheel, workspace)
        assert caught.value is builder_error

    assert builder_init_base_executables == [original_base_executable]
    assert builder_base_executables == [resolved_base_executable]
    assert verifier.sys._base_executable == original_base_executable
