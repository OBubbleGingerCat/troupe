#!/usr/bin/env bash
set -uo pipefail

repository_root="$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd -P)"

usage() {
  printf 'usage: %s --all --locked --deny-warnings\n' "${0##*/}" >&2
}

if [[ $# -ne 3 || "$1" != --all || "$2" != --locked || "$3" != --deny-warnings ]]; then
  usage
  exit 2
fi

if [[ "$(git -C "$repository_root" rev-parse --show-toplevel 2>/dev/null)" != "$repository_root" ]]; then
  printf 'Rust quality runner requires the repository root checkout\n' >&2
  exit 1
fi

python_executable="$(command -v python3 || command -v python)" || {
  printf 'Rust quality runner requires Python for PyO3 tests\n' >&2
  exit 1
}
python_executable="$(readlink -f -- "$python_executable")" || exit 1
if [[ ! -f "$python_executable" || ! -x "$python_executable" ]]; then
  printf 'Rust quality Python must resolve to an executable regular file\n' >&2
  exit 1
fi
python_prefix="$(
  env -u PYTHONHOME "$python_executable" -c \
    'import sys; print(sys.base_prefix)'
)" || exit 1
python_library="$(
  env -u PYTHONHOME "$python_executable" -c \
    'import sysconfig; print(sysconfig.get_config_var("LIBDIR") or "")'
)" || exit 1
if [[ "$python_prefix" != /* || ! -d "$python_prefix" || -L "$python_prefix" ]]; then
  printf 'Rust quality Python base prefix must be an absolute real directory\n' >&2
  exit 1
fi
if [[ "$python_library" != /* || ! -d "$python_library" || -L "$python_library" ]]; then
  printf 'Rust quality Python library path must be an absolute real directory\n' >&2
  exit 1
fi
python_prefix="$(CDPATH= cd -- "$python_prefix" && pwd -P)" || exit 1
python_library="$(CDPATH= cd -- "$python_library" && pwd -P)" || exit 1
export PYO3_PYTHON="$python_executable"
export PYTHONHOME="$python_prefix"
export LD_LIBRARY_PATH="$python_library${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"

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
temporary_base="${TMPDIR:-/tmp}"
temporary_base="$(CDPATH= cd -- "$temporary_base" && pwd -P)" || exit 1
quality_root="$(mktemp -d -- "$temporary_base/troupe-rust-quality.XXXXXX")" || exit 1
ownership_marker="$quality_root/.troupe-rust-quality-owned"
: > "$ownership_marker"

cleanup() {
  local original_status=$?
  trap - EXIT
  if [[ -n "$quality_root" && "$quality_root" == "$temporary_base"/troupe-rust-quality.* \
        && -f "$ownership_marker" ]]; then
    rm -rf -- "$quality_root"
  else
    printf 'refusing to clean unowned Rust quality path: %s\n' "$quality_root" >&2
    if [[ $original_status -eq 0 ]]; then
      original_status=1
    fi
  fi
  exit "$original_status"
}
trap cleanup EXIT

export CARGO_NET_OFFLINE=true
export CARGO_TERM_COLOR=never
export HTTP_PROXY=http://127.0.0.1:9/
export HTTPS_PROXY=http://127.0.0.1:9/
export http_proxy=http://127.0.0.1:9/
export https_proxy=http://127.0.0.1:9/
export ALL_PROXY=http://127.0.0.1:9/
export all_proxy=http://127.0.0.1:9/
export NO_PROXY=localhost,127.0.0.1,::1

declare -a stage_names=()
declare -a stage_statuses=()
declare -a stage_codes=()
declare -a stage_stdout_hashes=()
declare -a stage_stderr_hashes=()
first_failed_stage=""
first_exit=0

run_stage() {
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
    if [[ -z "$first_failed_stage" ]]; then
      first_failed_stage=$name
      first_exit=$exit_code
    fi
  fi
  stage_names+=("$name")
  stage_statuses+=("$status")
  stage_codes+=("$exit_code")
  stage_stdout_hashes+=("$(sha256sum "$stdout_path" | cut -d ' ' -f 1)")
  stage_stderr_hashes+=("$(sha256sum "$stderr_path" | cut -d ' ' -f 1)")
}

run_stage fmt \
  cargo fmt --check --all --manifest-path rust/Cargo.toml
run_stage check \
  cargo check --locked --offline --manifest-path rust/Cargo.toml \
    --workspace --all-targets --all-features
run_stage clippy \
  cargo clippy --locked --offline --manifest-path rust/Cargo.toml \
    --workspace --all-targets --all-features -- -D warnings
run_stage test \
  cargo test --locked --offline --manifest-path rust/Cargo.toml \
    --workspace --all-targets --all-features --no-fail-fast

final_checkout="$(checkout_fingerprint)" || final_checkout=""
checkout_unchanged=true
if [[ -z "$final_checkout" || "$final_checkout" != "$initial_checkout" ]]; then
  checkout_unchanged=false
  if [[ -z "$first_failed_stage" ]]; then
    first_failed_stage=checkout
    first_exit=1
  fi
fi

if [[ -z "$first_failed_stage" ]]; then
  result=passed
  first_failed_json=null
else
  result=failed
  first_failed_json="\"$first_failed_stage\""
fi

printf '{"schema":"troupe.diagnostics.rust-quality-result.v1",'
printf '"mode":"all","locked":true,"offline":true,"deny_warnings":true,'
printf '"result":"%s","first_failed_stage":%s,' "$result" "$first_failed_json"
printf '"checkout_unchanged":%s,"stages":[' "$checkout_unchanged"
for index in "${!stage_names[@]}"; do
  if [[ $index -ne 0 ]]; then
    printf ','
  fi
  printf '{"name":"%s","status":"%s","exit_code":%s,' \
    "${stage_names[$index]}" "${stage_statuses[$index]}" "${stage_codes[$index]}"
  printf '"stdout_sha256":"%s","stderr_sha256":"%s"}' \
    "${stage_stdout_hashes[$index]}" "${stage_stderr_hashes[$index]}"
done
printf ']}\n'

exit "$first_exit"
