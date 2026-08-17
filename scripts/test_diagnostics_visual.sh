#!/usr/bin/env bash
set -euo pipefail

repository_root="$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd -P)"
baseline_root="$repository_root/frontend/diagnostics/tests/e2e/visual/baselines"
manifest="$repository_root/frontend/diagnostics/tests/e2e/visual/screenshot-manifest.json"

usage() {
  printf 'usage: %s --all-engines --forbid-update --npm-cache CACHE --browser-cache CACHE\n' \
    "${0##*/}" >&2
}

if [[ $# -ne 6 || "$1" != --all-engines || "$2" != --forbid-update \
      || "$3" != --npm-cache || "$5" != --browser-cache ]]; then
  usage
  exit 2
fi
if [[ -n "${TROUPE_CAPTURE_VISUAL_BASELINES-}" ]]; then
  printf 'visual release runner forbids baseline capture or update\n' >&2
  exit 1
fi

baseline_fingerprint() {
  sha256sum "$manifest" "$baseline_root"/*.png | sha256sum | cut -d ' ' -f 1
}

initial_baselines="$(baseline_fingerprint)"
export TROUPE_VISUAL_FORBID_UPDATE=1
node "$repository_root/frontend/diagnostics/scripts/maintain.mjs" \
  --npm-cache "$4" \
  --typecheck \
  --browser tests/e2e/visual/diagnostics.spec.ts \
  --browser-cache "$6"
final_baselines="$(baseline_fingerprint)"
if [[ "$final_baselines" != "$initial_baselines" ]]; then
  printf 'visual release runner observed a baseline mutation\n' >&2
  exit 1
fi
