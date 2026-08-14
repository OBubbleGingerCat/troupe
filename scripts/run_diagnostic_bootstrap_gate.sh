#!/usr/bin/env bash
set -euo pipefail

repository_root="$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd -P)"
export PYTHONDONTWRITEBYTECODE=1
exec python "$repository_root/tests/support/diagnostic_bootstrap_gate.py" "$@"
