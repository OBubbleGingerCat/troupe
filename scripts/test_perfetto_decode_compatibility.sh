#!/usr/bin/env bash
set -euo pipefail

repository_root="$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd -P)"

if [[ "$#" -ne 1 || "$1" != "--offline" ]]; then
  echo "usage: $0 --offline" >&2
  exit 2
fi

export PYTHONDONTWRITEBYTECODE=1
exec python "$repository_root/tests/perfetto/decode/decoder.py" \
  --offline \
  --root "$repository_root"
