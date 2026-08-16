from __future__ import annotations

import hashlib
import json
import os
import shutil
import stat
import subprocess
import sys
from pathlib import Path

import pytest


ROOT = Path(__file__).resolve().parents[2]
SCRIPT_RELATIVE = Path("scripts/test_perfetto_compatibility.sh")
SCRIPT = ROOT / SCRIPT_RELATIVE
TRACE_MANIFEST_RELATIVE = Path("tests/fixtures/perfetto/traces/manifest.json")
LAYERS = {
    "decode": "test_perfetto_decode_compatibility.sh",
    "sql": "test_perfetto_sql_compatibility.sh",
    "ui": "test_perfetto_ui_compatibility.sh",
}
SHA = {
    "perfetto_manifest": "1" * 64,
    "tools": "2" * 64,
    "ui": "3" * 64,
    "browser_manifest": "4" * 64,
    "lock": "5" * 64,
    "browsers": "6" * 64,
    "chromium_archive": "7" * 64,
    "chromium_tree": "8" * 64,
    "chromium_executable": "9" * 64,
}


FAKE_LAYER = """#!/usr/bin/env python3
import json
import os
import sys
from pathlib import Path

name = Path(sys.argv[0]).name
layer = {
    "test_perfetto_decode_compatibility.sh": "decode",
    "test_perfetto_sql_compatibility.sh": "sql",
    "test_perfetto_ui_compatibility.sh": "ui",
    "fetch_pinned_perfetto_tools.sh": "canary",
}[name]
record = {
    "layer": layer,
    "argv": sys.argv[1:],
    "HTTP_PROXY": os.environ.get("HTTP_PROXY"),
    "http_proxy": os.environ.get("http_proxy"),
    "network_forbidden": os.environ.get("TROUPE_PERFETTO_NETWORK_FORBIDDEN"),
    "npm_offline": os.environ.get("npm_config_offline"),
}
with Path(os.environ["TROUPE_LAYER_LOG"]).open("a", encoding="utf-8") as stream:
    stream.write(json.dumps(record, sort_keys=True) + "\\n")
print(f"{layer} stdout")
print(f"{layer} stderr", file=sys.stderr)
failures = json.loads(os.environ.get("TROUPE_LAYER_FAILURES", "{}"))
raise SystemExit(int(failures.get(layer, 0)))
"""


def _write_json(path: Path, value: object) -> None:
    path.write_text(json.dumps(value, sort_keys=True) + "\n", encoding="utf-8")


def _copy_harness(tmp_path: Path) -> tuple[Path, Path, Path]:
    root = tmp_path / "repository"
    scripts = root / "scripts"
    scripts.mkdir(parents=True)
    shutil.copy2(SCRIPT, root / SCRIPT_RELATIVE)
    for name in (*LAYERS.values(), "fetch_pinned_perfetto_tools.sh"):
        path = scripts / name
        path.write_text(FAKE_LAYER, encoding="utf-8")
        path.chmod(0o755)
    trace_manifest = root / TRACE_MANIFEST_RELATIVE
    trace_manifest.parent.mkdir(parents=True)
    shutil.copy2(ROOT / TRACE_MANIFEST_RELATIVE, trace_manifest)

    perfetto = tmp_path / "perfetto-cache"
    perfetto.mkdir()
    _write_json(
        perfetto / ".troupe-perfetto-cache.json",
        {
            "schema_version": 1,
            "release_tag": "v57.2",
            "release_commit": "commit",
            "platform": "linux-amd64",
            "manifest_sha256": SHA["perfetto_manifest"],
            "members": [
                {"kind": "tools", "cache_filename": "tools.zip", "sha256": SHA["tools"]},
                {"kind": "ui", "cache_filename": "ui.zip", "sha256": SHA["ui"]},
            ],
            "test_only": False,
            "test_manifest": None,
        },
    )
    browser = tmp_path / "browser-cache"
    browser.mkdir()
    _write_json(
        browser / ".troupe-playwright-cache.json",
        {
            "schemaVersion": 1,
            "manifestSha256": SHA["browser_manifest"],
            "lockSha256": SHA["lock"],
            "platform": "linux-x64",
            "playwrightPlatform": "ubuntu22.04-x64",
            "playwrightCore": {
                "version": "1.62.1",
                "integrity": "sha512-test",
                "browsersSha256": SHA["browsers"],
            },
            "archives": [
                {
                    "name": "chromium",
                    "revision": "1234",
                    "browserVersion": "151.0.7922.34",
                    "cacheDirectory": "chromium-1234",
                    "archiveSha256": SHA["chromium_archive"],
                    "treeSha256": SHA["chromium_tree"],
                    "memberCount": 1,
                    "executable": "chrome/chrome",
                    "executableSha256": SHA["chromium_executable"],
                    "materializedLinks": [],
                }
            ],
        },
    )
    return root.resolve(strict=True), perfetto.resolve(strict=True), browser.resolve(strict=True)


def _run(
    root: Path,
    arguments: list[str],
    *,
    failures: dict[str, int] | None = None,
) -> tuple[subprocess.CompletedProcess[str], list[dict[str, object]]]:
    log = root.parent / "layers.jsonl"
    environment = dict(os.environ)
    environment["TROUPE_LAYER_LOG"] = str(log)
    environment["TROUPE_LAYER_FAILURES"] = json.dumps(failures or {})
    completed = subprocess.run(
        [str(root / SCRIPT_RELATIVE), *arguments],
        cwd=root,
        env=environment,
        check=False,
        capture_output=True,
        text=True,
        timeout=20,
    )
    records = [] if not log.exists() else [json.loads(line) for line in log.read_text().splitlines()]
    return completed, records


def _summary(completed: subprocess.CompletedProcess[str]) -> dict[str, object]:
    return json.loads(completed.stdout)


def test_all_layers_run_in_order_with_hash_bound_summary(tmp_path: Path) -> None:
    root, perfetto, browser = _copy_harness(tmp_path)
    completed, records = _run(
        root,
        [
            "--offline",
            "--all-layers",
            "--cache",
            str(perfetto),
            "--browser-cache",
            str(browser),
        ],
    )

    assert completed.returncode == 0, completed.stderr
    assert [record["layer"] for record in records] == ["decode", "sql", "ui"]
    assert [record["argv"] for record in records] == [
        ["--offline"],
        ["--offline", "--cache", str(perfetto)],
        ["--offline", "--cache", str(perfetto), "--browser-cache", str(browser)],
    ]
    assert all(record["HTTP_PROXY"] == "http://127.0.0.1:9/" for record in records)
    assert all(record["http_proxy"] == "http://127.0.0.1:9/" for record in records)
    assert all(record["network_forbidden"] == "1" for record in records)
    assert all(record["npm_offline"] == "true" for record in records)

    summary = _summary(completed)
    manifest_bytes = (root / TRACE_MANIFEST_RELATIVE).read_bytes()
    source_manifest = json.loads(manifest_bytes)
    assert summary["schema"] == "troupe.perfetto.compatibility.v1"
    assert summary["mode"] == "all-layers"
    assert summary["result"] == "passed"
    assert summary["first_failed_layer"] is None
    assert summary["fixtures"] == {
        "manifest_sha256": hashlib.sha256(manifest_bytes).hexdigest(),
        "files": [
            {"path": entry["path"], "sha256": entry["sha256"]}
            for entry in source_manifest["files"]
        ],
    }
    assert summary["perfetto"]["manifest_sha256"] == SHA["perfetto_manifest"]
    assert summary["perfetto"]["members"] == [
        {"kind": "tools", "sha256": SHA["tools"]},
        {"kind": "ui", "sha256": SHA["ui"]},
    ]
    assert summary["browser"]["chromium"]["executable_sha256"] == SHA["chromium_executable"]
    assert [entry["status"] for entry in summary["layers"]] == ["passed", "passed", "passed"]


@pytest.mark.parametrize(
    ("selector", "expected", "needs_perfetto", "needs_browser"),
    [
        ("--decode", "decode", False, False),
        ("--sql", "sql", True, False),
        ("--ui", "ui", True, True),
    ],
)
def test_single_layer_modes_forward_only_complete_arguments(
    tmp_path: Path,
    selector: str,
    expected: str,
    needs_perfetto: bool,
    needs_browser: bool,
) -> None:
    root, perfetto, browser = _copy_harness(tmp_path)
    arguments = ["--offline", selector]
    expected_argv = ["--offline"]
    if needs_perfetto:
        arguments.extend(("--cache", str(perfetto)))
        expected_argv.extend(("--cache", str(perfetto)))
    if needs_browser:
        arguments.extend(("--browser-cache", str(browser)))
        expected_argv.extend(("--browser-cache", str(browser)))

    completed, records = _run(root, arguments)

    assert completed.returncode == 0, completed.stderr
    assert records == [
        {
            "layer": expected,
            "argv": expected_argv,
            "HTTP_PROXY": "http://127.0.0.1:9/",
            "http_proxy": "http://127.0.0.1:9/",
            "network_forbidden": "1",
            "npm_offline": "true",
        }
    ]
    summary = _summary(completed)
    assert summary["mode"] == expected
    assert (summary["perfetto"] is not None) is needs_perfetto
    assert (summary["browser"] is not None) is needs_browser
    assert [entry["status"] for entry in summary["layers"]] == [
        "passed" if layer == expected else "not_selected" for layer in ("decode", "sql", "ui")
    ]


def test_all_layers_report_every_result_and_preserve_first_nonzero(tmp_path: Path) -> None:
    root, perfetto, browser = _copy_harness(tmp_path)
    completed, records = _run(
        root,
        [
            "--offline",
            "--all-layers",
            "--cache",
            str(perfetto),
            "--browser-cache",
            str(browser),
        ],
        failures={"sql": 127, "ui": 42},
    )

    assert completed.returncode == 127
    assert [record["layer"] for record in records] == ["decode", "sql", "ui"]
    summary = _summary(completed)
    assert summary["result"] == "failed"
    assert summary["first_failed_layer"] == "sql"
    assert [(entry["status"], entry["exit_code"]) for entry in summary["layers"]] == [
        ("passed", 0),
        ("failed", 127),
        ("failed", 42),
    ]
    assert "Perfetto sql layer failed" in completed.stderr
    assert "Perfetto ui layer failed" in completed.stderr


@pytest.mark.parametrize(
    "arguments",
    [
        ["--decode"],
        ["--offline", "--decode", "--cache", "/tmp/cache"],
        ["--offline", "--sql"],
        ["--offline", "--sql", "--cache", "/tmp/cache", "--browser-cache", "/tmp/browser"],
        ["--offline", "--ui", "--cache", "/tmp/cache"],
        ["--offline", "--all-layers", "--cache", "/tmp/cache"],
        ["--current-public-ui-canary", "--offline"],
        ["--decode", "--sql"],
    ],
)
def test_arguments_are_closed(tmp_path: Path, arguments: list[str]) -> None:
    root, _perfetto, _browser = _copy_harness(tmp_path)
    completed, records = _run(root, arguments)

    assert completed.returncode == 2
    assert records == []


def test_current_public_ui_canary_is_separate_from_release_result(tmp_path: Path) -> None:
    root, _perfetto, _browser = _copy_harness(tmp_path)
    completed, records = _run(root, ["--current-public-ui-canary"])

    assert completed.returncode == 0
    assert records[0]["layer"] == "canary"
    assert records[0]["argv"] == ["--current-public-ui-canary"]
    assert "troupe.perfetto.compatibility.v1" not in completed.stdout


def test_missing_layer_fails_before_any_layer_runs(tmp_path: Path) -> None:
    root, perfetto, browser = _copy_harness(tmp_path)
    (root / "scripts" / LAYERS["sql"]).unlink()

    completed, records = _run(
        root,
        [
            "--offline",
            "--all-layers",
            "--cache",
            str(perfetto),
            "--browser-cache",
            str(browser),
        ],
    )

    assert completed.returncode == 1
    assert records == []
    assert "sql compatibility layer must be an existing regular file" in completed.stderr


def test_source_and_wheel_inventory_exclude_compatibility_caches_and_binaries() -> None:
    sys.path.insert(0, str(ROOT / "tests/support"))
    from artifact_layout import expected_package_members, load_artifact_layout

    layout = load_artifact_layout(ROOT)
    expected = set(expected_package_members(layout, ".py"))
    expected.update(expected_package_members(layout, ".pyi"))
    expected.add("py.typed")
    tracked = subprocess.run(
        ["git", "ls-files", "src/troupe"],
        cwd=ROOT,
        check=True,
        capture_output=True,
        text=True,
    ).stdout.splitlines()
    actual = {path.removeprefix("src/troupe/") for path in tracked}

    assert actual == expected
    forbidden = ("perfetto", "trace_processor", "chromium", "playwright", ".zip", ".wasm")
    assert all(not any(token in member.lower() for token in forbidden) for member in actual)
    assert stat.S_IMODE(SCRIPT.stat().st_mode) == 0o755
