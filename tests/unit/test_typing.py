from __future__ import annotations

import os
import re
import subprocess
import sys
from collections import Counter
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
NEGATIVE = ROOT / "tests" / "typing" / "negative.py"


def _run(cache_dir: Path, module: str, *args: str) -> subprocess.CompletedProcess[str]:
    env = os.environ.copy()
    env["MYPY_CACHE_DIR"] = str(cache_dir)
    return subprocess.run(
        [sys.executable, "-m", module, *args],
        cwd=ROOT,
        env=env,
        capture_output=True,
        text=True,
        check=False,
    )


def test_runtime_stub_and_typed_consumers(tmp_path: Path) -> None:
    stubtest = _run(tmp_path / "stubtest", "mypy.stubtest", "troupe", "--concise")
    assert stubtest.returncode == 0, stubtest.stdout + stubtest.stderr
    assert stubtest.stdout == ""
    assert stubtest.stderr == ""

    positive = _run(
        tmp_path / "positive",
        "mypy",
        "--strict",
        "--show-error-codes",
        "tests/typing/positive.py",
        "examples/hello_actor/production.py",
        "examples/actor_pipeline/production.py",
        "examples/cooperative_workers/production.py",
        "examples/cancellation_cleanup/production.py",
    )
    assert positive.returncode == 0, positive.stdout + positive.stderr

    negative = _run(
        tmp_path / "negative",
        "mypy",
        "--strict",
        "--show-error-codes",
        "tests/typing/negative.py",
    )
    assert negative.returncode != 0
    diagnostics = Counter(
        (int(match.group("line")), match.group("code"))
        for line in negative.stdout.splitlines()
        if (
            match := re.match(
                r"^tests/typing/negative\.py:(?P<line>\d+): error: .*"
                r"\[(?P<code>[a-z-]+)\]$",
                line,
            )
        )
    )
    expected = Counter()
    for line_number, line in enumerate(
        NEGATIVE.read_text(encoding="utf-8").splitlines(),
        start=1,
    ):
        if marker := re.search(r"# E: (?P<code>[a-z-]+)$", line):
            expected[(line_number, marker.group("code"))] += 1
    assert diagnostics == expected, negative.stdout + negative.stderr
