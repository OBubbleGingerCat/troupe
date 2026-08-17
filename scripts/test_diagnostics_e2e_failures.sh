#!/usr/bin/env bash
set -euo pipefail

repository_root="$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd -P)"
python_executable="$(command -v python3 || command -v python)"

export PYTHONDONTWRITEBYTECODE=1
exec "$python_executable" -B "$repository_root/tests/e2e/diagnostics_failures/runner.py" "$@"
