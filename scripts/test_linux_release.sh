#!/usr/bin/env bash
set -euo pipefail

repository_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)"
cd -- "$repository_root"

temporary_base="${TMPDIR:-/tmp}"
temporary_base="$(cd -- "$temporary_base" && pwd -P)"
declare -a temporary_roots=()

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

  cargo fmt --check --manifest-path rust/Cargo.toml
  cargo clippy --locked --manifest-path rust/Cargo.toml --all-targets --all-features -- -D warnings
  PYTHONHOME="$managed_prefix" cargo test --locked --manifest-path rust/Cargo.toml
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
  printf 'usage: %s [quality|build|compatibility|all]\n' "${0##*/}" >&2
}

main() {
  if (($# > 1)); then
    _usage
    return 2
  fi

  case "${1:-all}" in
    quality)
      quality
      ;;
    build)
      build
      ;;
    compatibility)
      compatibility
      ;;
    all)
      quality
      build
      compatibility
      ;;
    *)
      _usage
      return 2
      ;;
  esac

  _audit_checkout
}

main "$@"
