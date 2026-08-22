#!/usr/bin/env bash
set -euo pipefail

# Build the release wheel once, then exercise that exact artifact in every
# supported GIL-enabled CPython minor version inside isolated containers.

repository_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)"
cd -- "$repository_root"

declare -a versions=(3.10 3.11 3.12 3.13 3.14)
builder_image="ghcr.io/pyo3/maturin:v1.14.1"
python_image_prefix="python"
temporary_base="${TMPDIR:-/tmp}"
temporary_base="$(cd -- "$temporary_base" && pwd -P)"
temporary_root=""

cleanup() {
  local status=$?
  trap - EXIT
  if [[ -n "$temporary_root" && -f "$temporary_root/.troupe-docker-matrix-owned" ]]; then
    rm -rf -- "$temporary_root"
  fi
  exit "$status"
}
trap cleanup EXIT

_ensure_image() {
  local image=$1
  if docker image inspect "$image" >/dev/null 2>&1; then
    return 0
  fi
  docker pull --platform linux/amd64 "$image"
}

if ! command -v docker >/dev/null 2>&1; then
  printf 'docker is required for the Python compatibility matrix\n' >&2
  exit 1
fi

temporary_root="$(mktemp -d -- "$temporary_base/troupe-python-matrix.XXXXXX")"
: > "$temporary_root/.troupe-docker-matrix-owned"
mkdir -- "$temporary_root/wheel-tools"

printf 'Preparing official CPython images: %s\n' "${versions[*]}"
for version in "${versions[@]}"; do
  _ensure_image "${python_image_prefix}:${version}-bookworm"
done
_ensure_image "$builder_image"

# Wheel is pure Python. Copy only that locked development dependency into the
# temporary mount so every container can run with --network=none.
wheel_python="${repository_root}/.venv/bin/python"
if [[ ! -x "$wheel_python" ]]; then
  wheel_python="$(command -v python3 || true)"
fi
if [[ -z "$wheel_python" || ! -x "$wheel_python" ]]; then
  printf 'a Python interpreter with the wheel package is required\n' >&2
  exit 1
fi
wheel_source="$($wheel_python -c 'import pathlib, wheel; print(pathlib.Path(wheel.__file__).resolve().parent)')"
wheel_dist_info="$($wheel_python -c 'import importlib.metadata; print(importlib.metadata.distribution("wheel")._path)')"
wheel_version="$($wheel_python -c 'import wheel; print(wheel.__version__)')"
if [[ "$wheel_version" != "0.47.0" ]]; then
  printf 'expected wheel 0.47.0, found %s\n' "$wheel_version" >&2
  exit 1
fi
cp -a -- "$wheel_source" "$temporary_root/wheel-tools/wheel"
cp -a -- "$wheel_dist_info" "$temporary_root/wheel-tools/"

declare -a cargo_mount=()
declare -a cargo_environment=()
cargo_user_home="$(getent passwd "$(id -u)" | cut -d: -f6 || true)"
if [[ -n "$cargo_user_home" && -d "$cargo_user_home/.cargo/registry" ]]; then
  cargo_mount=(-v "$cargo_user_home/.cargo/registry:/root/.cargo/registry:ro")
  cargo_environment=(--network=none -e CARGO_NET_OFFLINE=true)
else
  cargo_environment=(--network=default)
fi

printf 'Building and validating the manylinux wheel in %s\n' "$builder_image"
docker run --rm --platform linux/amd64 "${cargo_environment[@]}" \
  --entrypoint /bin/bash \
  -w /io \
  -v "$repository_root:/io:ro" \
  -v "$temporary_root:/matrix" \
  -v "$temporary_root/wheel-tools:/matrix/wheel-tools:ro" \
  "${cargo_mount[@]}" \
  "$builder_image" \
  -euo pipefail -c '
    PYTHONPATH=/matrix/wheel-tools \
      CARGO_TARGET_DIR=/tmp/troupe-cargo-target \
      /opt/python/cp311-cp311/bin/python scripts/verify_wheel.py \
        --build \
        --release \
        --target x86_64-unknown-linux-gnu \
        --manylinux 2_17 \
        --output-dir /matrix/wheel-artifact
  '

shopt -s nullglob
built_wheels=("$temporary_root/wheel-artifact/"*.whl)
shopt -u nullglob
if [[ ${#built_wheels[@]} -ne 1 || ! -f "$temporary_root/wheel-artifact/SHA256SUMS" ]]; then
  printf 'Docker build did not publish exactly one wheel and SHA256SUMS\n' >&2
  exit 1
fi
wheel_name="$(basename -- "${built_wheels[0]}")"

for version in "${versions[@]}"; do
  printf '\nTesting CPython %s\n' "$version"
  docker run --rm --platform linux/amd64 --network=none \
    -w /repo \
    -e PYTHONDONTWRITEBYTECODE=1 \
    -e PIP_ROOT_USER_ACTION=ignore \
    -e MATRIX_WHEEL="$wheel_name" \
    -e PYTHONPATH=/matrix/wheel-tools \
    -v "$repository_root:/repo:ro" \
    -v "$temporary_root:/matrix:ro" \
    "${python_image_prefix}:${version}-bookworm" \
    sh -ec '
      python --version
      python /repo/scripts/verify_wheel.py \
        --wheel "/matrix/wheel-artifact/$MATRIX_WHEEL" \
        --sha256-file /matrix/wheel-artifact/SHA256SUMS
    '
done

printf '\nDocker Python compatibility matrix passed: %s\n' "${versions[*]}"
