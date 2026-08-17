#!/usr/bin/env bash
set -euo pipefail

repository_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)"
cd -- "$repository_root"

temporary_base="${TMPDIR:-/tmp}"
temporary_base="$(cd -- "$temporary_base" && pwd -P)"
declare -a temporary_roots=()
diagnostics_evidence_root=""

_cleanup() {
  local original_status=$?
  local cleanup_failed=0
  local root

  trap - EXIT
  for root in "${temporary_roots[@]}"; do
    if [[ -z "$root" || ! -f "$root/.troupe-release-owned" ]]; then
      printf 'refusing to clean unowned temporary path: %s\n' "$root" >&2
      cleanup_failed=1
      continue
    fi
    if ! rm -rf -- "$root"; then
      cleanup_failed=1
    fi
  done

  if ((original_status != 0)); then
    exit "$original_status"
  fi
  exit "$cleanup_failed"
}
trap _cleanup EXIT

_create_temporary_root() {
  local prefix=$1
  local output_variable=$2
  local root

  root="$(mktemp -d -- "$temporary_base/${prefix}.XXXXXX")"
  : > "$root/.troupe-release-owned"
  temporary_roots+=("$root")
  printf -v "$output_variable" '%s' "$root"
}

_require_external_directory() {
  local value=$1
  local label=$2
  local output_variable=$3
  local canonical

  if [[ "$value" != /* || ! -d "$value" || -L "$value" ]]; then
    printf '%s must be an absolute real directory\n' "$label" >&2
    return 1
  fi
  canonical="$(cd -- "$value" && pwd -P)"
  if [[ "$canonical" != "$value" ]]; then
    printf '%s must be a normalized path without symlink indirection\n' "$label" >&2
    return 1
  fi
  case "$canonical/" in
    "$repository_root/"*)
      printf '%s must remain outside the repository\n' "$label" >&2
      return 1
      ;;
  esac
  printf -v "$output_variable" '%s' "$canonical"
}

_require_cache() {
  local name=$1
  local value=${!name-}
  local resolved

  if [[ -z "$value" ]]; then
    printf 'diagnostics release requires %s\n' "$name" >&2
    return 1
  fi
  _require_external_directory "$value" "$name" resolved
  printf -v "$name" '%s' "$resolved"
  export "$name"
}

_run_release_child() {
  local label=$1
  local timeout_seconds=$2
  local gate_tmp=$3
  shift 3
  local -a command=("$@")
  local -a environment=(env -u TROUPE_GATE_TMP)

  if [[ -n "$gate_tmp" ]]; then
    environment=(env "TROUPE_GATE_TMP=$gate_tmp")
  fi
  if [[ ${TROUPE_DIAGNOSTIC_BOOTSTRAP_GATE-} == 1 ]]; then
    local python_executable
    python_executable="$(command -v python3 || command -v python)"
    command=(
      "$python_executable"
      -B
      "$repository_root/tests/unit/test_release_script.py"
      --bootstrap-fake-child
      "$label"
      "$timeout_seconds"
      "$gate_tmp"
      --
      "${command[@]}"
    )
  fi
  "${environment[@]}" timeout --foreground --signal=TERM --kill-after=10s \
    "${timeout_seconds}s" "${command[@]}"
}

_quality_for_python() {
  export PYTHONDONTWRITEBYTECODE=1
  uv sync --frozen --all-groups

  local resolved_python
  local managed_prefix
  local managed_library
  resolved_python="$(readlink -f -- "$PYO3_PYTHON")"
  managed_prefix="$(dirname -- "$(dirname -- "$resolved_python")")"
  managed_library="$managed_prefix/lib"
  if [[ ! -d "$managed_library" ]]; then
    printf 'managed Python library directory does not exist: %s\n' \
      "$managed_library" >&2
    return 1
  fi
  export LD_LIBRARY_PATH="$managed_library${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"

  cargo fmt --check --all --manifest-path rust/Cargo.toml
  cargo clippy --locked --manifest-path rust/Cargo.toml --workspace --all-targets --all-features -- -D warnings
  PYTHONHOME="$managed_prefix" cargo test --locked --manifest-path rust/Cargo.toml --workspace
  env -u CONDA_PREFIX uv run --no-sync maturin develop --uv --locked \
    --features agent-test-support --manifest-path rust/Cargo.toml
  uv run --no-sync pytest -q
  uv run --no-sync python -m mypy --strict --show-error-codes tests/typing/positive.py
  uv run --no-sync python -m mypy.stubtest troupe --concise
  uv run --no-sync python -m doctest README.md
}

quality() {
  local -a versions=(3.10 3.14)
  local quality_root
  local version

  uv python install "${versions[@]}"
  _create_temporary_root troupe-quality quality_root

  for version in "${versions[@]}"; do
    (
      export UV_PYTHON="$version"
      export UV_PROJECT_ENVIRONMENT="$quality_root/$version"
      export UV_PYTHON_PREFERENCE=only-managed
      export PYO3_PYTHON="$UV_PROJECT_ENVIRONMENT/bin/python"
      _quality_for_python
    )
  done
}

build() {
  local uv_executable
  uv_executable="$(readlink -f -- "$(command -v uv)")"

  docker run \
    --rm \
    --entrypoint /bin/bash \
    -w /io \
    -v "$repository_root:/io" \
    -v "$uv_executable:/usr/local/bin/uv:ro" \
    -e UV_PYTHON=/opt/python/cp310-cp310/bin/python \
    -e UV_PROJECT_ENVIRONMENT=/tmp/troupe-venv \
    ghcr.io/pyo3/maturin:v1.14.1 \
    -euo pipefail -c '
      /usr/local/bin/uv sync --frozen --all-groups --no-install-project &&
      test ! -e /io/wheel-artifact &&
      /usr/local/bin/uv run --no-sync python scripts/verify_wheel.py --build --release --target x86_64-unknown-linux-gnu --manylinux 2_17 --output-dir wheel-artifact
    '
}

compatibility() {
  local -a wheels=()
  local -a versions=(3.10 3.11 3.12 3.13 3.14)
  local compatibility_root
  local version
  local wheel

  shopt -s nullglob
  wheels=(wheel-artifact/*.whl)
  shopt -u nullglob
  if [[ ${#wheels[@]} -ne 1 || ! -f "${wheels[0]-}" ]]; then
    printf 'wheel-artifact must contain exactly one wheel\n' >&2
    return 1
  fi
  if [[ ! -f wheel-artifact/SHA256SUMS ]]; then
    printf 'wheel-artifact/SHA256SUMS does not exist\n' >&2
    return 1
  fi
  wheel="${wheels[0]}"

  uv python install "${versions[@]}"
  _create_temporary_root troupe-compatibility compatibility_root

  for version in "${versions[@]}"; do
    (
      export UV_PYTHON="$version"
      export UV_PROJECT_ENVIRONMENT="$compatibility_root/$version"
      export UV_PYTHON_PREFERENCE=only-managed
      unset PYO3_PYTHON
      uv sync --frozen --all-groups --no-install-project
      uv run --no-sync python scripts/verify_wheel.py \
        --wheel "$wheel" \
        --sha256-file wheel-artifact/SHA256SUMS
    )
  done
}

diagnostics() {
  local v05_root
  local v07_root

  _require_cache TROUPE_NPM_CACHE
  _require_cache TROUPE_PLAYWRIGHT_CACHE
  _require_cache TROUPE_PERFETTO_CACHE

  if [[ -n "$diagnostics_evidence_root" ]]; then
    v05_root=$diagnostics_evidence_root
    v07_root=$diagnostics_evidence_root
  else
    _create_temporary_root troupe-diagnostics-v05 v05_root
    _create_temporary_root troupe-diagnostics-v07 v07_root
  fi

  _run_release_child V00 900 "" scripts/run_diagnostic_node_gate.sh V00
  _run_release_child V04 900 "" scripts/run_diagnostic_node_gate.sh V04
  _run_release_child V13 900 "" scripts/run_diagnostic_node_gate.sh V13
  _run_release_child V05 900 "$v05_root" scripts/run_diagnostic_node_gate.sh V05
  _run_release_child V07 1800 "$v07_root" \
    scripts/test_diagnostics_wheel.sh \
      --offline \
      --smoke active,archive \
      --report "$v07_root/V07-wheel-report.json"
  _run_release_child V08 1800 "" \
    scripts/test_diagnostics_python_compat.sh \
      --versions 3.10,3.11,3.12,3.13,3.14 \
      --build-current-wheel-once
  _run_release_child V09 900 "" \
    scripts/test_diagnostics_frontend_release.sh \
      --clean \
      --check-generated \
      --forbid-update \
      --npm-cache "$TROUPE_NPM_CACHE"
  _run_release_child V10 1800 "" \
    scripts/test_diagnostics_rust_quality.sh --all --locked --deny-warnings
  _run_release_child V14 1800 "" scripts/run_diagnostic_node_gate.sh V14
  _run_release_child V15 900 "" \
    scripts/test_diagnostics_perfetto_release.sh \
      --offline \
      --all-layers \
      --perfetto-cache "$TROUPE_PERFETTO_CACHE" \
      --browser-cache "$TROUPE_PLAYWRIGHT_CACHE"
}

_audit_checkout() {
  local checkout_uid
  local fixture_caches
  local wrong_owners

  git diff --check

  fixture_caches="$(
    find "$repository_root/tests/fixtures/productions" \
      -type d -name __pycache__ -print
  )"
  if [[ -n "$fixture_caches" ]]; then
    printf 'fixture checkout contains __pycache__ directories:\n%s\n' \
      "$fixture_caches" >&2
    return 1
  fi

  checkout_uid="$(stat -c %u "$repository_root")"
  wrong_owners="$(find "$repository_root" ! -uid "$checkout_uid" -print)"
  if [[ -n "$wrong_owners" ]]; then
    printf 'checkout contains paths owned by another uid:\n%s\n' \
      "$wrong_owners" >&2
    return 1
  fi
}

_usage() {
  printf 'usage: %s [quality|build|compatibility|diagnostics|all] [--diagnostics-evidence-root DIR]\n' "${0##*/}" >&2
}

main() {
  local mode=${1:-all}

  if [[ "$mode" == -h || "$mode" == --help || "$mode" == help ]]; then
    if (($# != 1)); then
      _usage
      return 2
    fi
    _usage
    return 0
  fi
  if (($# == 3)) && [[ "$mode" == all && "$2" == --diagnostics-evidence-root ]]; then
    _require_external_directory "$3" "diagnostics evidence root" diagnostics_evidence_root
    if [[ -n "$(find "$diagnostics_evidence_root" -mindepth 1 -maxdepth 1 -print -quit)" ]]; then
      printf 'diagnostics evidence root must be a fresh empty directory\n' >&2
      return 1
    fi
  elif (($# > 1)); then
    _usage
    return 2
  fi

  if [[ ${TROUPE_DIAGNOSTIC_BOOTSTRAP_GATE-} == 1 && "$mode" == all ]]; then
    _run_release_child quality 60 "" quality
    _run_release_child build 60 "" build
    _run_release_child compatibility 60 "" compatibility
    diagnostics
    _audit_checkout
    return
  fi

  case "$mode" in
    quality)
      quality
      ;;
    build)
      build
      ;;
    compatibility)
      compatibility
      ;;
    diagnostics)
      diagnostics
      ;;
    all)
      quality
      build
      compatibility
      diagnostics
      ;;
    *)
      _usage
      return 2
      ;;
  esac

  _audit_checkout
}

main "$@"
