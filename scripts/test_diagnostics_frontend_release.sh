#!/usr/bin/env bash
set -uo pipefail

repository_root="$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd -P)"

usage() {
  printf 'usage: %s --clean --check-generated --forbid-update --npm-cache CACHE\n' "${0##*/}" >&2
}

if [[ $# -ne 5 || "$1" != --clean || "$2" != --check-generated \
      || "$3" != --forbid-update || "$4" != --npm-cache ]]; then
  usage
  exit 2
fi
npm_cache=$5

if [[ "$(git -C "$repository_root" rev-parse --show-toplevel 2>/dev/null)" != "$repository_root" ]]; then
  printf 'Frontend release runner requires the repository root checkout\n' >&2
  exit 1
fi
if [[ "$npm_cache" != /* || ! -d "$npm_cache" || -L "$npm_cache" ]]; then
  printf 'Frontend release runner requires an absolute real npm cache directory\n' >&2
  exit 1
fi
npm_cache="$(CDPATH= cd -- "$npm_cache" && pwd -P)" || exit 1
case "$npm_cache/" in
  "$repository_root/"*)
    printf 'Frontend release npm cache must remain outside the repository\n' >&2
    exit 1
    ;;
esac

frontend_root="$repository_root/frontend/diagnostics"
for forbidden in "$frontend_root/node_modules" "$frontend_root/dist"; do
  if [[ -e "$forbidden" || -L "$forbidden" ]]; then
    printf 'Frontend release requires a clean source tree without %s\n' "$forbidden" >&2
    exit 1
  fi
done

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

if [[ -n "$(git -C "$repository_root" status --porcelain=v1 --untracked-files=all)" ]]; then
  printf 'Frontend release runner requires a clean tracked checkout\n' >&2
  exit 1
fi
initial_checkout="$(checkout_fingerprint)" || exit 1

temporary_base="${TROUPE_GATE_TMP:-/tmp}"
temporary_base="$(CDPATH= cd -- "$temporary_base" && pwd -P)" || exit 1
case "$temporary_base/" in
  "$repository_root/"*)
    printf 'Frontend release temporary base must remain outside the repository\n' >&2
    exit 1
    ;;
esac
release_root="$(mktemp -d -- "$temporary_base/troupe-frontend-release.XXXXXX")" || exit 1
ownership_marker="$release_root/.troupe-frontend-release-owned"
if ! (umask 077 && : > "$ownership_marker"); then
  rmdir -- "$release_root" 2>/dev/null || true
  exit 1
fi

cleanup() {
  local original_status=$?
  trap - EXIT
  if [[ -n "$release_root" && "$release_root" == "$temporary_base"/troupe-frontend-release.* \
        && -f "$ownership_marker" ]]; then
    if ! rm -rf -- "$release_root" && [[ $original_status -eq 0 ]]; then
      original_status=1
    fi
  else
    printf 'refusing to clean unowned frontend release path: %s\n' "$release_root" >&2
    if [[ $original_status -eq 0 ]]; then
      original_status=1
    fi
  fi
  exit "$original_status"
}
trap cleanup EXIT

if ! mkdir -m 700 -- "$release_root/blocked-bin"; then
  exit 1
fi
for command in node npm npx; do
  if ! printf '#!/usr/bin/env bash\nprintf "forbidden frontend tool during Rust build: %%s\\n" "$0" >&2\nexit 97\n' \
      > "$release_root/blocked-bin/$command" \
      || ! chmod 700 -- "$release_root/blocked-bin/$command"; then
    exit 1
  fi
done

export CARGO_NET_OFFLINE=true
export HTTP_PROXY=http://127.0.0.1:9/
export HTTPS_PROXY=http://127.0.0.1:9/
export http_proxy=http://127.0.0.1:9/
export https_proxy=http://127.0.0.1:9/
export ALL_PROXY=http://127.0.0.1:9/
export all_proxy=http://127.0.0.1:9/
export NO_PROXY=localhost,127.0.0.1,::1
export no_proxy=localhost,127.0.0.1,::1

unit_tests="tests/unit/browser-provisioning.test.ts,tests/unit/bundle-contract.test.ts,tests/unit/generated-assets.test.ts,tests/unit/protocol-controls.test.ts,tests/unit/protocol-events.test.ts,tests/unit/protocol-views.test.ts,tests/unit/state-property.test.ts,tests/unit/state-reducer.test.ts,tests/unit/state-windows.test.ts,tests/unit/timeline-hit-test.test.ts,tests/unit/timeline-layout.test.ts,tests/unit/toolchain.test.ts"

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
  local stdout_path="$release_root/$name.stdout"
  local stderr_path="$release_root/$name.stderr"
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

run_stage frontend \
  node frontend/diagnostics/scripts/maintain.mjs \
    --npm-cache "$npm_cache" \
    --check-toolchain \
    --typecheck \
    --unit "$unit_tests" \
    --build-raw \
    --verify-reproducible \
    --generate-assets \
    --check \
    --repeat 2

run_stage rust-embedded \
  env "PATH=$release_root/blocked-bin:$PATH" \
    cargo build --locked --offline --manifest-path rust/Cargo.toml \
      --package troupe-diagnostics-runtime --lib

for forbidden in "$frontend_root/node_modules" "$frontend_root/dist"; do
  if [[ -e "$forbidden" || -L "$forbidden" ]]; then
    if [[ -z "$first_failed_stage" ]]; then
      first_failed_stage=checkout
      first_exit=1
    fi
  fi
done
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

printf '{"schema":"troupe.diagnostics.frontend-release-result.v1",'
printf '"result":"%s","first_failed_stage":%s,' "$result" "$first_failed_json"
printf '"clean":true,"check_generated":true,"forbid_update":true,'
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
