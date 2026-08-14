from __future__ import annotations

import base64
import hashlib
import json
import os
import shutil
import stat
import subprocess
import sys
from pathlib import Path
from typing import Any

import pytest


ROOT = Path(__file__).resolve().parents[2]
FRONTEND = ROOT / "frontend" / "diagnostics"
MAINTAINER = FRONTEND / "scripts" / "maintain.mjs"
NODE_MAJOR = int(subprocess.run(
    ["node", "-p", "process.versions.node.split('.')[0]"],
    check=True,
    capture_output=True,
    text=True,
).stdout.strip())

RUNTIME_DEPENDENCIES = {
    "@preact/signals": "2.11.1",
    "lucide-preact": "1.31.0",
    "preact": "10.29.8",
    "uplot": "1.6.32",
}
DEVELOPMENT_DEPENDENCIES = {
    "@axe-core/playwright": "4.13.0",
    "@playwright/test": "1.62.1",
    "@preact/preset-vite": "2.10.6",
    "@testing-library/jest-dom": "7.0.1",
    "@testing-library/preact": "3.2.4",
    "@types/node": "26.2.0",
    "axe-core": "4.13.0",
    "jsdom": "28.1.0",
    "typescript": "7.0.2",
    "vite": "8.2.1",
    "vitest": "4.1.10",
}
BANNED_DEPENDENCIES = {
    "@vitejs/plugin-react",
    "@vitejs/plugin-react-swc",
    "d3",
    "echarts",
    "handlebars",
    "next",
    "react",
    "react-dom",
    "react-router",
    "react-router-dom",
    "redux",
    "sass",
    "styled-components",
    "tailwindcss",
    "vue",
    "@reduxjs/toolkit",
    "@tanstack/react-query",
}


def _json(path: Path) -> dict[str, Any]:
    value = json.loads(path.read_text(encoding="utf-8"))
    assert isinstance(value, dict)
    return value


def _run_maintainer(
    project: Path,
    *arguments: str,
    environment: dict[str, str] | None = None,
) -> subprocess.CompletedProcess[str]:
    current = dict(os.environ)
    if environment:
        current.update(environment)
    return subprocess.run(
        ["node", str(project / "scripts" / "maintain.mjs"), *arguments],
        cwd=ROOT,
        env=current,
        capture_output=True,
        text=True,
        check=False,
    )


def _lock_digest(project: Path) -> str:
    return hashlib.sha256((project / "package-lock.json").read_bytes()).hexdigest()


def _integrity_path(cache: Path, integrity: str) -> Path:
    algorithm, encoded = integrity.split("-", 1)
    assert algorithm == "sha512"
    digest = base64.b64decode(encoded, validate=True).hex()
    return cache / "_cacache" / "content-v2" / algorithm / digest[:2] / digest[2:4] / digest[4:]


def _write_json(path: Path, value: dict[str, Any]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(value, indent=2) + "\n", encoding="utf-8")


def _copy_maintainer_project(tmp_path: Path) -> Path:
    project = tmp_path / "frontend"
    (project / "scripts").mkdir(parents=True)
    shutil.copy2(MAINTAINER, project / "scripts" / "maintain.mjs")
    (project / ".node-version").write_text(f"{NODE_MAJOR}\n", encoding="utf-8")
    package = {
        "name": "troupe-diagnostics-maintainer-probe",
        "version": "0.0.0",
        "private": True,
        "type": "module",
        "packageManager": "npm@10.9.4",
        "engines": {"node": ">=22 <23"},
        "dependencies": {},
        "devDependencies": {},
    }
    lock = {
        "name": package["name"],
        "version": package["version"],
        "lockfileVersion": 3,
        "requires": True,
        "packages": {"": package},
    }
    _write_json(project / "package.json", package)
    _write_json(project / "package-lock.json", lock)
    return project


def _write_fake_npm(tmp_path: Path) -> tuple[Path, Path]:
    binary = tmp_path / "bin" / "npm"
    log = tmp_path / "npm-calls.jsonl"
    binary.parent.mkdir()
    binary.write_text(
        f'''#!{sys.executable}
import json
import os
from pathlib import Path
import sys

arguments = sys.argv[1:]
with Path(os.environ["FAKE_NPM_LOG"]).open("a", encoding="utf-8") as stream:
    stream.write(json.dumps(arguments) + "\\n")
if arguments == ["--version"]:
    print("10.9.4")
    raise SystemExit(0)
if not arguments or arguments[0] != "ci":
    raise SystemExit(91)
prefix = Path(arguments[arguments.index("--prefix") + 1])
bin_dir = prefix / "node_modules" / ".bin"
bin_dir.mkdir(parents=True)
for name in ("tsc", "vitest"):
    target = bin_dir / name
    target.write_text("#!/bin/sh\\nexit 0\\n", encoding="utf-8")
    target.chmod(0o755)
vite = bin_dir / "vite"
vite.write_text(
    """#!/bin/sh
out=""
while [ "$#" -gt 0 ]; do
  if [ "$1" = "--outDir" ]; then out="$2"; shift 2; else shift; fi
done
mkdir -p "$out/assets"
printf '<div id="app"></div>\\n' > "$out/index.html"
printf 'export{{}};\\n' > "$out/assets/diagnostics-probe.js"
printf ':root{{color:black}}\\n' > "$out/assets/diagnostics-probe.css"
""",
    encoding="utf-8",
)
vite.chmod(0o755)
''',
        encoding="utf-8",
    )
    binary.chmod(binary.stat().st_mode | stat.S_IXUSR)
    return binary.parent, log


def test_package_and_lock_pin_the_exact_supported_toolchain() -> None:
    package = _json(FRONTEND / "package.json")
    lock = _json(FRONTEND / "package-lock.json")

    assert package["private"] is True
    assert package["type"] == "module"
    assert package["packageManager"] == "npm@10.9.4"
    assert package["engines"] == {"node": ">=22 <23"}
    assert package["dependencies"] == RUNTIME_DEPENDENCIES
    assert package["devDependencies"] == DEVELOPMENT_DEPENDENCIES
    assert not (BANNED_DEPENDENCIES & set(package["dependencies"]))
    assert not (BANNED_DEPENDENCIES & set(package["devDependencies"]))
    assert all(not version.startswith(("^", "~", ">", "<", "*")) for version in {
        *package["dependencies"].values(),
        *package["devDependencies"].values(),
    })

    assert lock["lockfileVersion"] == 3
    assert lock["packages"][""]["dependencies"] == RUNTIME_DEPENDENCIES
    assert lock["packages"][""]["devDependencies"] == DEVELOPMENT_DEPENDENCIES
    locked_paths = set(lock["packages"])
    assert all(
        not any(
            path == f"node_modules/{dependency}"
            or path.endswith(f"/node_modules/{dependency}")
            for path in locked_paths
        )
        for dependency in BANNED_DEPENDENCIES
    )
    for path, entry in lock["packages"].items():
        if not path or entry.get("link"):
            continue
        assert entry["resolved"].startswith("https://registry.npmjs.org/")
        algorithm, encoded = entry["integrity"].split("-", 1)
        assert algorithm == "sha512"
        assert len(base64.b64decode(encoded, validate=True)) == 64


def test_frontend_configuration_is_strict_relative_and_single_chunk() -> None:
    typescript = _json(FRONTEND / "tsconfig.json")["compilerOptions"]
    assert typescript["strict"] is True
    assert typescript["noUncheckedIndexedAccess"] is True
    assert typescript["exactOptionalPropertyTypes"] is True
    assert typescript["target"] == "ES2020"
    assert typescript["jsx"] == "react-jsx"
    assert typescript["jsxImportSource"] == "preact"
    assert typescript["noEmit"] is True

    vite = (FRONTEND / "vite.config.ts").read_text(encoding="utf-8")
    assert 'base: "./"' in vite
    assert 'target: "es2020"' in vite
    assert "cssCodeSplit: false" in vite
    assert "inlineDynamicImports: true" in vite
    assert "sourcemap: false" in vite
    assert "manualChunks" not in vite

    assert (FRONTEND / ".node-version").read_text(encoding="utf-8") == "22\n"
    assert "@vitejs/plugin-react" not in vite
    assert not (FRONTEND / "node_modules").exists()
    assert not (FRONTEND / "dist").exists()


def test_maintainer_fails_closed_for_cache_and_registry_authority(tmp_path: Path) -> None:
    cache = tmp_path / "cache"
    cache.mkdir()

    missing = _run_maintainer(FRONTEND, "--check-toolchain")
    assert missing.returncode != 0
    assert "--npm-cache is required" in missing.stderr

    relative = _run_maintainer(FRONTEND, "--npm-cache", "relative", "--check-toolchain")
    assert relative.returncode != 0
    assert "absolute" in relative.stderr

    home = tmp_path / "home"
    home_cache = home / ".npm" / "cache"
    home_cache.mkdir(parents=True)
    implicit_home = _run_maintainer(
        FRONTEND,
        "--npm-cache",
        str(home_cache),
        "--check-toolchain",
        environment={"HOME": str(home)},
    )
    assert implicit_home.returncode != 0
    assert "home npm cache" in implicit_home.stderr

    undeclared_registry = _run_maintainer(
        FRONTEND,
        "--npm-cache",
        str(cache),
        "--check-toolchain",
        environment={"npm_config_registry": "https://registry.invalid.example/"},
    )
    assert undeclared_registry.returncode != 0
    assert "registry authority requires --allow-registry" in undeclared_registry.stderr

    (cache / "unexpected").write_text("occupied", encoding="utf-8")
    stale = _run_maintainer(
        FRONTEND,
        "--npm-cache",
        str(cache),
        "--allow-registry",
        "--verify-offline-cache-replay",
    )
    assert stale.returncode != 0
    assert "fresh empty npm cache" in stale.stderr


def test_fake_registry_install_replays_offline_without_browser_cache(tmp_path: Path) -> None:
    project = _copy_maintainer_project(tmp_path)
    fake_bin, log = _write_fake_npm(tmp_path)
    cache = tmp_path / "cache"
    cache.mkdir()
    environment = {
        "PATH": f"{fake_bin}{os.pathsep}{os.environ['PATH']}",
        "FAKE_NPM_LOG": str(log),
    }
    environment.pop("PLAYWRIGHT_BROWSERS_PATH", None)

    completed = _run_maintainer(
        project,
        "--npm-cache",
        str(cache),
        "--allow-registry",
        "--check-toolchain",
        "--unit",
        "--verify-offline-cache-replay",
        environment=environment,
    )
    assert completed.returncode == 0, completed.stderr

    calls = [json.loads(line) for line in log.read_text(encoding="utf-8").splitlines()]
    installs = [call for call in calls if call and call[0] == "ci"]
    assert len(installs) == 2
    assert "--offline" not in installs[0]
    assert "--offline" in installs[1]
    assert all("--ignore-scripts" in call for call in installs)
    assert all(call[call.index("--cache") + 1] == str(cache) for call in installs)
    assert not (project / "node_modules").exists()
    assert not (project / "dist").exists()
    manifest = _json(cache / ".troupe-npm-cache.json")
    assert manifest["lockSha256"] == _lock_digest(project)
    assert manifest["members"] == []


def test_fake_package_cache_provision_is_atomic_and_read_only(tmp_path: Path) -> None:
    project = _copy_maintainer_project(tmp_path)
    fake_bin, log = _write_fake_npm(tmp_path)
    cache = tmp_path / "published-cache"
    environment = {
        "PATH": f"{fake_bin}{os.pathsep}{os.environ['PATH']}",
        "FAKE_NPM_LOG": str(log),
    }

    try:
        completed = _run_maintainer(
            project,
            "--npm-cache",
            str(cache),
            "--provision-package-cache",
            "--allow-registry",
            environment=environment,
        )
        assert completed.returncode == 0, completed.stderr
        assert cache.is_dir()
        assert cache.stat().st_mode & 0o222 == 0
        assert (cache / ".troupe-npm-cache.json").stat().st_mode & 0o222 == 0
        assert _json(cache / ".troupe-npm-cache.json")["lockSha256"] == _lock_digest(project)
        assert not list(tmp_path.glob(".published-cache.staging-*"))
        calls = [json.loads(line) for line in log.read_text(encoding="utf-8").splitlines()]
        installs = [call for call in calls if call and call[0] == "ci"]
        assert len(installs) == 2
        assert "--offline" not in installs[0]
        assert "--offline" in installs[1]
    finally:
        if cache.exists():
            cache.chmod(0o755)
            for path in cache.rglob("*"):
                path.chmod(0o755 if path.is_dir() else 0o644)


def test_lock_node_and_cached_integrity_mismatch_fail_before_install(tmp_path: Path) -> None:
    project = _copy_maintainer_project(tmp_path)
    cache = tmp_path / "cache"
    cache.mkdir()

    package = _json(project / "package.json")
    package["dependencies"] = {"probe": "1.0.0"}
    _write_json(project / "package.json", package)
    mismatch = _run_maintainer(project, "--npm-cache", str(cache), "--check-toolchain")
    assert mismatch.returncode != 0
    assert "package-lock root does not match package.json" in mismatch.stderr

    package["dependencies"] = {}
    _write_json(project / "package.json", package)
    (project / ".node-version").write_text(f"{NODE_MAJOR + 1}\n", encoding="utf-8")
    node_mismatch = _run_maintainer(project, "--npm-cache", str(cache), "--check-toolchain")
    assert node_mismatch.returncode != 0
    assert "Node major mismatch" in node_mismatch.stderr

    (project / ".node-version").write_text(f"{NODE_MAJOR}\n", encoding="utf-8")
    payload = b"expected package tarball"
    integrity = "sha512-" + base64.b64encode(hashlib.sha512(payload).digest()).decode("ascii")
    package["dependencies"] = {"probe": "1.0.0"}
    lock = _json(project / "package-lock.json")
    lock["packages"][""]["dependencies"] = package["dependencies"]
    lock["packages"]["node_modules/probe"] = {
        "version": "1.0.0",
        "resolved": "https://registry.npmjs.org/probe/-/probe-1.0.0.tgz",
        "integrity": integrity,
    }
    _write_json(project / "package.json", package)
    _write_json(project / "package-lock.json", lock)
    cached = _integrity_path(cache, integrity)
    cached.parent.mkdir(parents=True)
    cached.write_bytes(b"corrupt")
    _write_json(
        cache / ".troupe-npm-cache.json",
        {
            "schemaVersion": 1,
            "lockSha256": _lock_digest(project),
            "nodeMajor": NODE_MAJOR,
            "nodeVersion": subprocess.run(
                ["node", "-p", "process.versions.node"],
                check=True,
                capture_output=True,
                text=True,
            ).stdout.strip(),
            "npmVersion": "10.9.4",
            "members": [
                {
                    "integrity": integrity,
                    "path": cached.relative_to(cache).as_posix(),
                    "resolved": lock["packages"]["node_modules/probe"]["resolved"],
                    "sha512": hashlib.sha512(payload).hexdigest(),
                    "size": len(payload),
                }
            ],
        },
    )
    corrupt = _run_maintainer(project, "--npm-cache", str(cache), "--check-toolchain")
    assert corrupt.returncode != 0
    assert "cached tarball integrity mismatch" in corrupt.stderr


@pytest.mark.parametrize(
    "arguments,expected",
    [
        (("--component", "tests/component/example.test.tsx"), "component"),
        (("--browser", "tests/e2e/example.spec.ts"), "--browser-cache is required"),
        (("--build-raw", "--verify-reproducible"), "build"),
        (("--generate-assets", "--check", "--repeat", "2"), "generate"),
    ],
)
def test_generic_dispatcher_recognizes_downstream_modes(
    tmp_path: Path,
    arguments: tuple[str, ...],
    expected: str,
) -> None:
    project = _copy_maintainer_project(tmp_path)
    cache = tmp_path / "cache"
    cache.mkdir()
    completed = _run_maintainer(project, "--npm-cache", str(cache), *arguments)
    assert completed.returncode != 2
    assert "unknown argument" not in completed.stderr
    assert expected in completed.stderr.lower()
