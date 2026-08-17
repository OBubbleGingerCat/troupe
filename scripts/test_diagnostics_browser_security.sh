#!/usr/bin/env bash
set -euo pipefail

repository_root="$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd -P)"

usage() {
  printf 'usage: %s --all-engines --npm-cache CACHE --browser-cache CACHE\n' "${0##*/}" >&2
}

if [[ $# -ne 5 || "$1" != --all-engines || "$2" != --npm-cache \
      || "$4" != --browser-cache ]]; then
  usage
  exit 2
fi

exec node "$repository_root/frontend/diagnostics/scripts/maintain.mjs" \
  --npm-cache "$3" \
  --typecheck \
  --browser tests/e2e/security/security.spec.ts \
  --browser-cache "$5"
