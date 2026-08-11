from __future__ import annotations

import asyncio
import gc
import importlib
import importlib.util
import json
import subprocess
import sys
import time
from pathlib import Path
from typing import Any

import pytest


ROOT = Path(__file__).resolve().parents[2]
MOCK_AGENT = ROOT / "tests" / "support" / "mock_acp_agent.py"
PRODUCTION_SOURCE = (
    ROOT / "examples" / "live_agents" / "mixed_repository_repair" / "production.py"
)
HARNESS_TIMEOUT = 5.0


def _native() -> Any:
    return importlib.import_module("troupe._runtime")


def _git(repository: Path, *args: str) -> str:
    result = subprocess.run(
        ["git", *args],
        cwd=repository,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        check=True,
        text=True,
    )
    return result.stdout.strip()


def _create_defective_repository(repository: Path, investigation_id: str) -> str:
    repository.mkdir()
    (repository / "repair.py").write_text(
        "def normalize_title(value: str) -> str:\n"
        "    return value\n",
        encoding="utf-8",
    )
    (repository / "test_repair.py").write_text(
        "import unittest\n\n"
        "from repair import normalize_title\n\n\n"
        "class RepairTests(unittest.TestCase):\n"
        "    def test_normalizes_title(self) -> None:\n"
        "        self.assertEqual(\n"
        "            normalize_title(\"  a tale OF TWO cities  \"),\n"
        "            \"A Tale Of Two Cities\",\n"
        "        )\n\n\n"
        "if __name__ == \"__main__\":\n"
        "    unittest.main()\n",
        encoding="utf-8",
    )
    (repository / "ISSUE.md").write_text(
        f"investigation-id: {investigation_id}\n"
        "normalize_title must strip surrounding whitespace and title-case each word.\n",
        encoding="utf-8",
    )
    (repository / ".gitignore").write_text("__pycache__/\n", encoding="utf-8")
    _git(repository, "init", "-q")
    _git(repository, "add", ".gitignore", "repair.py", "test_repair.py")
    _git(
        repository,
        "-c",
        "user.name=Troupe Test",
        "-c",
        "user.email=troupe@example.invalid",
        "commit",
        "-q",
        "-m",
        "baseline",
    )
    return _git(repository, "rev-parse", "HEAD")


def _load_production_type() -> type[Any]:
    spec = importlib.util.spec_from_file_location(
        "troupe_mixed_repository_repair_test",
        PRODUCTION_SOURCE,
    )
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module.Production


def _events(path: Path) -> list[dict[str, Any]]:
    if not path.is_file():
        return []
    return [json.loads(line) for line in path.read_text(encoding="utf-8").splitlines()]


def _process_identity(pid: int) -> str | None:
    try:
        fields = Path(f"/proc/{pid}/stat").read_text(encoding="ascii")
    except (FileNotFoundError, PermissionError, ProcessLookupError):
        return None
    suffix = fields.rsplit(")", 1)
    if len(suffix) != 2:
        return None
    values = suffix[1].split()
    return values[19] if len(values) > 19 else None


@pytest.fixture(autouse=True)
def _reset_test_launch() -> Any:
    _native()._agent_test_reset_launch()
    yield
    _native()._agent_test_reset_launch()


def test_mixed_production_uses_three_actor_sessions_and_codex_context(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    repository = tmp_path / "repository"
    report = tmp_path / "report.json"
    events = tmp_path / "events.jsonl"
    investigation_id = "investigation-context-7f31"
    baseline = _create_defective_repository(repository, investigation_id)
    for provider in ("codex", "claude", "kimi"):
        monkeypatch.setenv(
            f"TROUPE_LIVE_{provider.upper()}_PROFILE",
            json.dumps(
                {
                    "workspace": str(repository),
                    "model": "test-model",
                    "effort": "max",
                }
            ),
        )
    _native()._agent_test_set_launch(
        program=sys.executable,
        args=[
            str(MOCK_AGENT),
            "--events",
            str(events),
            "--scenario",
            "mixed_agents",
        ],
    )
    production_type = _load_production_type()
    runtime = _native()._Runtime()

    async def scenario() -> None:
        run = asyncio.ensure_future(
            runtime.run(production_type([str(repository), str(report)]))
        )
        while not report.is_file():
            assert not run.done()
            await asyncio.sleep(0)
        runtime.request_shutdown()
        await run

    asyncio.run(asyncio.wait_for(scenario(), HARNESS_TIMEOUT))

    started = [row for row in _events(events) if row["event"] == "process_started"]
    assert len(started) == 3
    identities = {
        row["pid"]: _process_identity(row["pid"])
        for row in started
    }
    assert all(identity is not None for identity in identities.values())
    del runtime
    gc.collect()
    cleanup_deadline = time.monotonic() + HARNESS_TIMEOUT
    while any(
        _process_identity(pid) == identity
        for pid, identity in identities.items()
    ):
        assert time.monotonic() < cleanup_deadline
        time.sleep(0.01)

    payload = json.loads(report.read_text(encoding="utf-8"))
    assert payload["investigation"] == {
        "expected_behavior": "strip surrounding whitespace and title-case each word",
        "investigation_id": investigation_id,
        "role": "investigator",
        "root_cause": "normalize_title returns its input unchanged",
        "target_file": "repair.py",
    }
    assert payload["review"] == {
        "approved": True,
        "contract": {
            "input": "arbitrary title text",
            "output": "trimmed title-cased text",
        },
        "role": "reviewer",
    }
    assert payload["recall"] == {
        "investigation_id": investigation_id,
        "remembered_root_cause": "normalize_title returns its input unchanged",
        "role": "investigator",
    }
    assert payload["repair"]["role"] == "implementer"
    assert payload["repair"]["changed_files"] == ["repair.py"]
    assert payload["repair"]["tests_passed"] is True
    assert payload["repair"]["commit"] == _git(repository, "rev-parse", "HEAD")
    assert payload["flow"] == [
        {"effect": "Investigation", "owner": "codex-investigator"},
        {"effect": "ContractReview", "owner": "claude-reviewer"},
        {"effect": "RepositoryRepair", "owner": "kimi-repairer"},
        {"effect": "ContextRecall", "owner": "codex-investigator"},
    ]

    assert not (repository / "ISSUE.md").exists()
    assert _git(repository, "rev-list", "--count", f"{baseline}..HEAD") == "1"
    assert _git(repository, "diff-tree", "--no-commit-id", "--name-only", "-r", "HEAD") == "repair.py"
    assert _git(repository, "log", "-1", "--pretty=%s") == "fix: normalize titles"
    assert _git(repository, "status", "--porcelain") == ""
    subprocess.run(
        [sys.executable, "-m", "unittest", "-q"],
        cwd=repository,
        check=True,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )

    rows = _events(events)
    providers = [
        row["provider"] for row in rows if row["event"] == "mixed_provider_identified"
    ]
    assert sorted(providers) == ["claude", "codex", "kimi"]
    prompts = [row for row in rows if row["event"] == "prompt_received"]
    codex_prompts = [row for row in prompts if row.get("provider") == "codex"]
    assert [row["turn"] for row in codex_prompts] == [1, 2]
    assert len({row["session_id"] for row in codex_prompts}) == 1
    assert investigation_id not in codex_prompts[1]["prompt"]
    assert sum(row["event"] == "stdin_closed" for row in rows) == 3
