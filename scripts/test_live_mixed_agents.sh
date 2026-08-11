#!/usr/bin/env bash
set -euo pipefail

repository_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)"
cd -- "$repository_root"

if (($# != 0)); then
  printf 'usage: %s\n' "${0##*/}" >&2
  exit 2
fi

build_root="$(mktemp -d "${TMPDIR:-/tmp}/troupe-live-mixed-build.XXXXXX")"
ownership_marker="$build_root/.troupe-live-build-owned"
: >"$ownership_marker"
cleanup() {
  if [[ -f "$ownership_marker" ]]; then
    rm -rf -- "$build_root"
  fi
}
trap cleanup EXIT

python_executable="$(uv run --no-sync python -c 'import sys; print(sys.executable)')"
env -u CONDA_PREFIX uv run --no-sync maturin build \
  --locked \
  --manifest-path rust/Cargo.toml \
  --out "$build_root/dist"
uv venv --offline --python "$python_executable" "$build_root/venv"
wheels=("$build_root"/dist/troupe-*.whl)
if ((${#wheels[@]} != 1)) || [[ ! -f "${wheels[0]}" ]]; then
  printf 'mixed live build did not produce exactly one troupe wheel\n' >&2
  exit 1
fi
uv pip install --offline --no-deps \
  --python "$build_root/venv/bin/python" \
  "${wheels[0]}"
env PYTHONDONTWRITEBYTECODE=1 \
  "$build_root/venv/bin/python" tests/live/mixed_agent_oracle.py
