from __future__ import annotations

import json
from pathlib import Path
from typing import Any

try:
    import tomllib
except ModuleNotFoundError:
    import tomli as tomllib


ROOT = Path(__file__).resolve().parents[2]
RUST = ROOT / "rust"
CRATES = RUST / "crates"

CORE = "troupe-diagnostics-core"
RUNTIME = "troupe-diagnostics-runtime"
PERFETTO = "troupe-diagnostics-perfetto"
AGENT = "troupe-agent-runtime"
DIAGNOSTICS_CRATES = (CORE, RUNTIME, PERFETTO)
INTERNAL_CRATES = frozenset((*DIAGNOSTICS_CRATES, AGENT))


def _toml(path: Path) -> dict[str, Any]:
    return tomllib.loads(path.read_text(encoding="utf-8"))


def _manifest(crate: str | None = None) -> dict[str, Any]:
    path = RUST / "Cargo.toml" if crate is None else CRATES / crate / "Cargo.toml"
    return _toml(path)


def _dependencies(manifest: dict[str, Any]) -> dict[str, Any]:
    return manifest.get("dependencies", {})


def _internal_dependencies(manifest: dict[str, Any]) -> set[str]:
    return set(_dependencies(manifest)) & INTERNAL_CRATES


def _path_dependency(manifest: dict[str, Any], name: str, expected: str) -> None:
    dependency = _dependencies(manifest)[name]
    assert isinstance(dependency, dict)
    assert dependency.get("path") == expected
    assert "version" not in dependency


def _exact_dependency(
    manifest: dict[str, Any],
    name: str,
    version: str,
    *,
    default_features: bool | None = None,
    features: list[str] | None = None,
) -> None:
    dependency = _dependencies(manifest)[name]
    assert isinstance(dependency, dict)
    assert dependency.get("version") == f"={version}"
    if default_features is not None:
        assert dependency.get("default-features", True) is default_features
    if features is not None:
        assert dependency.get("features", []) == features


def _lock_packages() -> list[dict[str, Any]]:
    return _toml(RUST / "Cargo.lock")["package"]


def _locked(name: str, version: str) -> dict[str, Any]:
    matches = [
        package
        for package in _lock_packages()
        if package["name"] == name and package["version"] == version
    ]
    assert len(matches) == 1
    return matches[0]


def test_workspace_registers_diagnostics_crates_and_native_edges() -> None:
    workspace = _manifest()
    assert workspace["workspace"]["members"] == [
        "crates/troupe-agent-runtime",
        "crates/troupe-diagnostics-core",
        "crates/troupe-diagnostics-runtime",
        "crates/troupe-diagnostics-perfetto",
    ]

    assert _internal_dependencies(workspace) == INTERNAL_CRATES
    _path_dependency(workspace, AGENT, "crates/troupe-agent-runtime")
    for crate in DIAGNOSTICS_CRATES:
        _path_dependency(workspace, crate, f"crates/{crate}")


def test_diagnostics_crate_ownership_and_dependency_direction() -> None:
    manifests = {crate: _manifest(crate) for crate in (*DIAGNOSTICS_CRATES, AGENT)}
    for crate in DIAGNOSTICS_CRATES:
        package = manifests[crate]["package"]
        assert package == {
            "name": crate,
            "version": "0.1.0",
            "edition": "2024",
            "publish": False,
        }

    assert _internal_dependencies(manifests[CORE]) == set()
    assert _internal_dependencies(manifests[RUNTIME]) == {CORE}
    assert _internal_dependencies(manifests[PERFETTO]) == {CORE, RUNTIME}
    assert _internal_dependencies(manifests[AGENT]) == {CORE}

    _path_dependency(manifests[RUNTIME], CORE, "../troupe-diagnostics-core")
    _path_dependency(manifests[PERFETTO], CORE, "../troupe-diagnostics-core")
    _path_dependency(manifests[PERFETTO], RUNTIME, "../troupe-diagnostics-runtime")
    _path_dependency(manifests[AGENT], CORE, "../troupe-diagnostics-core")


def test_required_direct_dependencies_are_exact_pinned_and_scoped() -> None:
    workspace = _manifest()
    runtime = _manifest(RUNTIME)
    perfetto = _manifest(PERFETTO)

    _exact_dependency(
        runtime,
        "rusqlite",
        "0.40.2",
        default_features=False,
        features=["bundled"],
    )
    _exact_dependency(
        runtime,
        "fs4",
        "1.1.0",
        default_features=False,
        features=["sync"],
    )
    _exact_dependency(runtime, "num-bigint", "0.5.1")
    _exact_dependency(
        workspace,
        "reqwest",
        "0.13.4",
        default_features=False,
        features=["rustls", "stream"],
    )
    _exact_dependency(perfetto, "prost", "0.14.4")

    manifests = {
        "rust/Cargo.toml": workspace,
        **{f"rust/crates/{crate}/Cargo.toml": _manifest(crate) for crate in (*DIAGNOSTICS_CRATES, AGENT)},
    }
    scopes = {
        dependency: {
            path
            for path, manifest in manifests.items()
            if dependency in _dependencies(manifest)
        }
        for dependency in ("rusqlite", "fs4", "num-bigint", "reqwest", "prost")
    }
    assert scopes == {
        "rusqlite": {f"rust/crates/{RUNTIME}/Cargo.toml"},
        "fs4": {f"rust/crates/{RUNTIME}/Cargo.toml"},
        "num-bigint": {f"rust/crates/{RUNTIME}/Cargo.toml"},
        "reqwest": {"rust/Cargo.toml"},
        "prost": {f"rust/crates/{PERFETTO}/Cargo.toml"},
    }

    forbidden = {
        "prost-build",
        "prost-types",
        "perfetto-protos",
        "perfetto_protos",
        "tracing-perfetto-file",
        "tracing-perfetto-sdk-schema",
        "napi",
        "neon",
        "node",
        "nodejs",
    }
    for path, manifest in manifests.items():
        declared: set[str] = set()
        for section in ("dependencies", "dev-dependencies", "build-dependencies"):
            declared.update(manifest.get(section, {}))
        assert declared.isdisjoint(forbidden), path


def test_agent_runtime_enables_only_end_turn_usage_feature() -> None:
    dependency = _dependencies(_manifest(AGENT))["agent-client-protocol"]
    assert dependency == {
        "version": "=2.0.0",
        "features": ["unstable_end_turn_token_usage"],
    }


def test_lockfile_contains_the_fixed_direct_dependency_graph() -> None:
    for crate in (*DIAGNOSTICS_CRATES, AGENT, "troupe"):
        package = _locked(crate, "0.1.0")
        assert "source" not in package

    assert set(_locked(AGENT, "0.1.0")["dependencies"]) >= {
        "agent-client-protocol",
        CORE,
    }
    assert set(_locked(RUNTIME, "0.1.0")["dependencies"]) >= {
        CORE,
        "fs4",
        "num-bigint",
        "rusqlite",
    }
    assert set(_locked(PERFETTO, "0.1.0")["dependencies"]) >= {
        CORE,
        RUNTIME,
        "prost",
    }
    assert set(_locked("troupe", "0.1.0")["dependencies"]) >= INTERNAL_CRATES | {
        "reqwest"
    }

    for name, version in (
        ("fs4", "1.1.0"),
        ("num-bigint", "0.5.1"),
        ("prost", "0.14.4"),
        ("reqwest", "0.13.4"),
        ("rusqlite", "0.40.2"),
    ):
        package = _locked(name, version)
        assert package["source"] == "registry+https://github.com/rust-lang/crates.io-index"
        assert len(package["checksum"]) == 64


def test_new_crate_roots_are_private_placeholders_until_f04() -> None:
    f04 = json.loads(
        (ROOT / "tests/fixtures/artifact_layout/nodes/F04.json").read_text(encoding="utf-8")
    )
    if f04["state"] != "planned":
        return

    for crate in DIAGNOSTICS_CRATES:
        source = (CRATES / crate / "src/lib.rs").read_text(encoding="utf-8")
        assert source == "#![allow(dead_code)]\n\nmod placeholder {}\n"
        assert "pub " not in source


def test_uv_cache_keys_cover_all_diagnostics_crate_inputs() -> None:
    pyproject = _toml(ROOT / "pyproject.toml")
    assert pyproject["build-system"]["requires"] == ["maturin==1.14.1"]
    cache_keys = pyproject["tool"]["uv"]["cache-keys"]
    assert {entry["file"] for entry in cache_keys} >= {
        "rust/crates/troupe-diagnostics-core/**/*",
        "rust/crates/troupe-diagnostics-runtime/**/*",
        "rust/crates/troupe-diagnostics-perfetto/**/*",
    }
