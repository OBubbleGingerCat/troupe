from __future__ import annotations

import re
import subprocess
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
SCRIPT = ROOT / "scripts" / "test_docker_python_matrix.sh"


def _script() -> str:
    return SCRIPT.read_text(encoding="utf-8")


def test_docker_python_matrix_script_is_executable_and_valid_shell() -> None:
    assert SCRIPT.stat().st_mode & 0o111
    completed = subprocess.run(
        ["bash", "-n", str(SCRIPT)],
        check=False,
        capture_output=True,
        text=True,
    )
    assert completed.returncode == 0, completed.stderr


def test_docker_python_matrix_covers_the_declared_release_contract() -> None:
    source = _script()

    assert re.search(r"versions=\(3\.10 3\.11 3\.12 3\.13 3\.14\)", source)
    assert 'python_image_prefix="python"' in source
    assert "--target x86_64-unknown-linux-gnu" in source
    assert "--manylinux 2 17" not in source
    assert "--manylinux 2_17" in source
    assert "--build" in source
    assert "--wheel \"/matrix/wheel-artifact/$MATRIX_WHEEL\"" in source
    assert "--sha256-file /matrix/wheel-artifact/SHA256SUMS" in source
    assert "--network=none" in source
    assert "docker image inspect" in source
    assert ":/repo:ro\"" in source
    assert ":/matrix:ro\"" in source
