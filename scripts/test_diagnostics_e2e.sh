#!/usr/bin/env bash
set -uo pipefail

repository_root="$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd -P)"

usage() {
  echo "usage: test_diagnostics_e2e.sh (--happy-path|--failures|--all)" >&2
}

if (( $# != 1 )); then
  usage
  exit 2
fi

node_gate=("$repository_root/scripts/run_diagnostic_node_gate.sh")
if [[ ${TROUPE_DIAGNOSTIC_BOOTSTRAP_GATE-} == 1 ]]; then
  python_executable="$(command -v python3 || command -v python)"
  node_gate=(
    "$python_executable"
    -B
    "$repository_root/tests/unit/test_diagnostics_e2e_assembly.py"
    --fake-child
  )
fi

case "$1" in
  --happy-path)
    exec "${node_gate[@]}" V02
    ;;
  --failures)
    exec "${node_gate[@]}" V06
    ;;
  --all)
    ;;
  *)
    usage
    exit 2
    ;;
esac

active_pid=""
interrupted=0

forward_signal() {
  local exit_code="$1"
  local signal_name="$2"
  if (( interrupted == 0 )); then
    interrupted="$exit_code"
  fi
  if [[ -n "$active_pid" ]]; then
    kill -s "$signal_name" "$active_pid" 2>/dev/null || true
  fi
}

trap 'forward_signal 130 INT' INT
trap 'forward_signal 143 TERM' TERM

first_failure=0
for node_id in V02 V06; do
  "${node_gate[@]}" "$node_id" &
  active_pid=$!
  wait "$active_pid"
  status=$?
  if (( interrupted != 0 )); then
    wait "$active_pid" 2>/dev/null || true
    active_pid=""
    exit "$interrupted"
  fi
  active_pid=""
  if (( status != 0 && first_failure == 0 )); then
    first_failure="$status"
  fi
done

exit "$first_failure"
