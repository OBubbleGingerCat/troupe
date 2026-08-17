#!/usr/bin/env bash
set -uo pipefail

repository_root="$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd -P)"

usage() {
  printf 'usage: %s {--all|--pytest|--mypy|--stubtest|--doctest}\n' "${0##*/}" >&2
}

if [[ $# -ne 1 ]]; then
  usage
  exit 2
fi

case "$1" in
  --all) selected_mode=all ;;
  --pytest) selected_mode=pytest ;;
  --mypy) selected_mode=mypy ;;
  --stubtest) selected_mode=stubtest ;;
  --doctest) selected_mode=doctest ;;
  *)
    usage
    exit 2
    ;;
esac

if [[ "$(git -C "$repository_root" rev-parse --show-toplevel 2>/dev/null)" != "$repository_root" ]]; then
  printf 'Python quality runner requires the repository root checkout\n' >&2
  exit 1
fi

checkout_fingerprint() {
  (
    cd -- "$repository_root" || exit 1
    git rev-parse HEAD
    git status --porcelain=v1 -z --untracked-files=all
    git diff --cached --no-ext-diff --no-textconv --binary
    git diff --no-ext-diff --no-textconv --binary
    while IFS= read -r -d '' relative_path; do
      printf 'untracked:%s\0' "$relative_path"
      if [[ -L "$relative_path" ]]; then
        printf 'symlink:'
        readlink -- "$relative_path"
      elif [[ -f "$relative_path" ]]; then
        printf 'file:'
        sha256sum -- "$relative_path"
      else
        printf 'other\n'
      fi
    done < <(git ls-files --others --exclude-standard -z)
  ) | sha256sum | cut -d ' ' -f 1
}

initial_checkout="$(checkout_fingerprint)" || exit 1
temporary_base="${TROUPE_GATE_TMP:-/tmp}"
temporary_base="$(CDPATH= cd -- "$temporary_base" && pwd -P)" || exit 1
case "$temporary_base/" in
  "$repository_root/"*)
    printf 'Python quality temporary base must remain outside the repository\n' >&2
    exit 1
    ;;
esac
quality_root="$(mktemp -d -- "$temporary_base/troupe-python-quality.XXXXXX")" || exit 1
ownership_marker="$quality_root/.troupe-python-quality-owned"
if ! (umask 077 && : > "$ownership_marker"); then
  rmdir -- "$quality_root" 2>/dev/null || true
  exit 1
fi
if ! mkdir -m 700 -- "$quality_root/tmp"; then
  rm -f -- "$ownership_marker"
  rmdir -- "$quality_root" 2>/dev/null || true
  exit 1
fi

cleanup() {
  local original_status=$?
  trap - EXIT
  if [[ -n "$quality_root" && "$quality_root" == "$temporary_base"/troupe-python-quality.* \
        && -f "$ownership_marker" ]]; then
    if ! rm -rf -- "$quality_root" && [[ $original_status -eq 0 ]]; then
      original_status=1
    fi
  else
    printf 'refusing to clean unowned Python quality path: %s\n' "$quality_root" >&2
    if [[ $original_status -eq 0 ]]; then
      original_status=1
    fi
  fi
  exit "$original_status"
}
trap cleanup EXIT

export PYTHONDONTWRITEBYTECODE=1
export PYTEST_ADDOPTS='-p no:cacheprovider'
export MYPY_CACHE_DIR="$quality_root/mypy-cache"
export TMPDIR="$quality_root/tmp"
export TMP="$quality_root/tmp"
export CARGO_NET_OFFLINE=true
export PIP_NO_INDEX=1
export UV_OFFLINE=1
export HTTP_PROXY=http://127.0.0.1:9/
export HTTPS_PROXY=http://127.0.0.1:9/
export http_proxy=http://127.0.0.1:9/
export https_proxy=http://127.0.0.1:9/
export ALL_PROXY=http://127.0.0.1:9/
export all_proxy=http://127.0.0.1:9/
export NO_PROXY=localhost,127.0.0.1,::1
export no_proxy=localhost,127.0.0.1,::1
unset PYTHONPATH

python_executable="$(command -v python 2>/dev/null || true)"
if [[ -z "$python_executable" ]]; then
  printf 'Python quality runner requires F03 isolated wheel environment\n' >&2
  exit 1
fi

"$python_executable" -B - "$repository_root" <<'PY'
from __future__ import annotations

import importlib
import importlib.machinery
import os
import shutil
import sys
from pathlib import Path


def fail(message: str) -> None:
    print(f"Python quality origin check: {message}", file=sys.stderr)
    raise SystemExit(1)


def required_path(name: str) -> Path:
    value = os.environ.get(name)
    if not value or not Path(value).is_absolute():
        fail(f"{name} must be an absolute F03 path")
    return Path(value).absolute()


def installed_file(value: object, name: str, venv: Path) -> Path:
    if not isinstance(value, str):
        fail(f"{name} has no installed file")
    path = Path(value).absolute()
    try:
        path.relative_to(venv)
    except ValueError:
        fail(f"{name} is outside the F03 venv")
    if path.is_symlink() or not path.is_file():
        fail(f"{name} is not a regular installed file")
    try:
        path.resolve(strict=True).relative_to(venv.resolve(strict=True))
    except (OSError, ValueError):
        fail(f"{name} resolves outside the F03 venv")
    return path


repository = Path(sys.argv[1]).resolve(strict=True)
venv = required_path("UV_PROJECT_ENVIRONMENT")
pyo3_python = required_path("PYO3_PYTHON")
if venv.is_symlink() or not venv.is_dir():
    fail("UV_PROJECT_ENVIRONMENT is not a regular directory")
if venv == (repository / ".venv").absolute():
    fail("the primary checkout .venv is forbidden")
expected_python = venv / "bin/python"
if pyo3_python != expected_python or Path(sys.executable).absolute() != expected_python:
    fail("sys.executable and PYO3_PYTHON must select the F03 venv Python")
if os.environ.get("PYTHONPATH"):
    fail("PYTHONPATH fallback is forbidden")

troupe = importlib.import_module("troupe")
runtime = importlib.import_module("troupe._runtime")
installed_file(troupe.__file__, "troupe module", venv)
runtime_file = installed_file(runtime.__file__, "native runtime", venv)
if not any(str(runtime_file).endswith(suffix) for suffix in importlib.machinery.EXTENSION_SUFFIXES):
    fail("troupe._runtime is not an installed native extension")
for command in ("pytest", "troupe"):
    installed_file(shutil.which(command), f"{command} command", venv)
PY
origin_exit=$?
if [[ $origin_exit -ne 0 ]]; then
  printf 'Python quality runner requires F03 isolated wheel environment\n' >&2
  exit "$origin_exit"
fi

declare -a mode_names=()
declare -a mode_statuses=()
declare -a mode_codes=()
declare -a mode_stdout_hashes=()
declare -a mode_stderr_hashes=()
first_failed_mode=""
first_exit=0

run_mode() {
  local name=$1
  shift
  local stdout_path="$quality_root/$name.stdout"
  local stderr_path="$quality_root/$name.stderr"
  local exit_code
  local status

  (
    cd -- "$repository_root" || exit 1
    "$@"
  ) >"$stdout_path" 2>"$stderr_path"
  exit_code=$?
  cat -- "$stdout_path" >&2
  cat -- "$stderr_path" >&2

  if [[ $exit_code -eq 0 ]]; then
    status=passed
  else
    status=failed
    if [[ -z "$first_failed_mode" ]]; then
      first_failed_mode=$name
      first_exit=$exit_code
    fi
  fi
  mode_names+=("$name")
  mode_statuses+=("$status")
  mode_codes+=("$exit_code")
  mode_stdout_hashes+=("$(sha256sum "$stdout_path" | cut -d ' ' -f 1)")
  mode_stderr_hashes+=("$(sha256sum "$stderr_path" | cut -d ' ' -f 1)")
}

if [[ "$selected_mode" == all || "$selected_mode" == pytest ]]; then
  run_mode pytest pytest -q
fi
if [[ "$selected_mode" == all || "$selected_mode" == mypy ]]; then
  run_mode mypy python -m mypy --strict --show-error-codes tests/typing/positive.py
fi
if [[ "$selected_mode" == all || "$selected_mode" == stubtest ]]; then
  run_mode stubtest python -m mypy.stubtest troupe --concise
fi
if [[ "$selected_mode" == all || "$selected_mode" == doctest ]]; then
  run_mode doctest python -m doctest README.md
fi

final_checkout="$(checkout_fingerprint)" || final_checkout=""
checkout_unchanged=true
if [[ -z "$final_checkout" || "$final_checkout" != "$initial_checkout" ]]; then
  checkout_unchanged=false
  if [[ -z "$first_failed_mode" ]]; then
    first_failed_mode=checkout
    first_exit=1
  fi
fi

if [[ -z "$first_failed_mode" ]]; then
  result=passed
  first_failed_json=null
else
  result=failed
  first_failed_json="\"$first_failed_mode\""
fi

printf '{"schema":"troupe.diagnostics.python-quality-result.v1",'
printf '"mode":"%s","isolated_origin":true,"offline":true,' "$selected_mode"
printf '"result":"%s","first_failed_mode":%s,' "$result" "$first_failed_json"
printf '"checkout_unchanged":%s,"modes":[' "$checkout_unchanged"
for index in "${!mode_names[@]}"; do
  if [[ $index -ne 0 ]]; then
    printf ','
  fi
  printf '{"name":"%s","status":"%s","exit_code":%s,' \
    "${mode_names[$index]}" "${mode_statuses[$index]}" "${mode_codes[$index]}"
  printf '"stdout_sha256":"%s","stderr_sha256":"%s"}' \
    "${mode_stdout_hashes[$index]}" "${mode_stderr_hashes[$index]}"
done
printf ']}\n'

exit "$first_exit"
