#!/usr/bin/env bash
set -euo pipefail

repository_root="$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd -P)"
baseline_relative="frontend/diagnostics/tests/stress/performance-baseline.json"
baseline_raw_relative="frontend/diagnostics/tests/stress/performance-baseline.raw.json"
review_relative="frontend/diagnostics/tests/stress/BASELINE_REVIEW.md"

usage() {
  printf 'usage: %s --repeat 3 --baseline %s --baseline-raw %s --review %s --raw-report REPORT --forbid-baseline-update --npm-cache CACHE --browser-cache CACHE\n' \
    "${0##*/}" "$baseline_relative" "$baseline_raw_relative" "$review_relative" >&2
}

fail() {
  printf 'diagnostics stress runner: %s\n' "$1" >&2
  exit 1
}

repeat=""
baseline=""
baseline_raw=""
review=""
raw_report=""
npm_cache=""
browser_cache=""
forbid_update=0

while [[ $# -gt 0 ]]; do
  case "$1" in
    --repeat|--baseline|--baseline-raw|--review|--raw-report|--npm-cache|--browser-cache)
      [[ $# -ge 2 && -n "$2" ]] || { usage; exit 2; }
      case "$1" in
        --repeat) repeat="$2" ;;
        --baseline) baseline="$2" ;;
        --baseline-raw) baseline_raw="$2" ;;
        --review) review="$2" ;;
        --raw-report) raw_report="$2" ;;
        --npm-cache) npm_cache="$2" ;;
        --browser-cache) browser_cache="$2" ;;
      esac
      shift 2
      ;;
    --forbid-baseline-update)
      [[ "$forbid_update" -eq 0 ]] || { usage; exit 2; }
      forbid_update=1
      shift
      ;;
    *)
      usage
      exit 2
      ;;
  esac
done

[[ "$repeat" == "3" ]] || fail "--repeat must be exactly 3"
[[ "$baseline" == "$baseline_relative" ]] || fail "--baseline must name the frozen repository baseline"
[[ "$baseline_raw" == "$baseline_raw_relative" ]] || fail "--baseline-raw must name the frozen repository calibration report"
[[ "$review" == "$review_relative" ]] || fail "--review must name the frozen repository review"
[[ "$forbid_update" -eq 1 ]] || fail "--forbid-baseline-update is required"
[[ -n "$raw_report" && -n "$npm_cache" && -n "$browser_cache" ]] || { usage; exit 2; }
[[ -z "${TROUPE_CAPTURE_PERFORMANCE_BASELINE-}" ]] || fail "baseline capture is forbidden"

baseline_path="$repository_root/$baseline"
baseline_raw_path="$repository_root/$baseline_raw"
review_path="$repository_root/$review"
schema_path="$repository_root/frontend/diagnostics/tests/stress/performance-raw.schema.json"
for frozen_path in "$baseline_path" "$baseline_raw_path" "$review_path" "$schema_path"; do
  [[ -f "$frozen_path" && ! -L "$frozen_path" ]] || fail "frozen input is missing, non-regular, or a symlink: $frozen_path"
done

[[ "$raw_report" == /* ]] || fail "--raw-report must be absolute"
[[ ! -e "$raw_report" && ! -L "$raw_report" ]] || fail "--raw-report must be create-new"
report_parent="$(realpath -e -- "$(dirname -- "$raw_report")")"
report_path="$report_parent/$(basename -- "$raw_report")"
case "$report_path" in
  "$repository_root"/*) fail "--raw-report must be outside the repository" ;;
esac
if [[ -n "${TROUPE_GATE_TMP-}" ]]; then
  gate_tmp="$(realpath -e -- "$TROUPE_GATE_TMP")"
  [[ "$report_parent" == "$gate_tmp" ]] || fail "--raw-report must be directly inside TROUPE_GATE_TMP"
fi

initial_baseline_sha="$(sha256sum "$baseline_path" | cut -d ' ' -f 1)"
initial_baseline_raw_sha="$(sha256sum "$baseline_raw_path" | cut -d ' ' -f 1)"
initial_review_sha="$(sha256sum "$review_path" | cut -d ' ' -f 1)"

verify_frozen_inputs() {
  local current_baseline_sha current_baseline_raw_sha current_review_sha
  for frozen_path in "$baseline_path" "$baseline_raw_path" "$review_path"; do
    [[ -f "$frozen_path" && ! -L "$frozen_path" ]] || return 1
  done
  current_baseline_sha="$(sha256sum "$baseline_path" | cut -d ' ' -f 1)"
  current_baseline_raw_sha="$(sha256sum "$baseline_raw_path" | cut -d ' ' -f 1)"
  current_review_sha="$(sha256sum "$review_path" | cut -d ' ' -f 1)"
  [[ "$current_baseline_sha" == "$initial_baseline_sha" \
      && "$current_baseline_raw_sha" == "$initial_baseline_raw_sha" \
      && "$current_review_sha" == "$initial_review_sha" ]]
}

lock_root="${XDG_RUNTIME_DIR:-/tmp}"
[[ -d "$lock_root" && ! -L "$lock_root" ]] || fail "benchmark lock root is unavailable"
lock_path="$lock_root/troupe-diagnostics-benchmark-host.${UID}.lock"
exec {benchmark_lock_fd}>"$lock_path"
flock -n "$benchmark_lock_fd" || fail "benchmark-host is already leased"
exclusive_started_ns="$(date +%s%N)"
lease_id_sha256="$(printf '%s\n' "benchmark-host:$UID:$$:$exclusive_started_ns:$report_path" | sha256sum | cut -d ' ' -f 1)"

export TROUPE_STRESS_REPEAT="$repeat"
export TROUPE_STRESS_REPORT_KIND="gate"
export TROUPE_STRESS_RAW_REPORT="$report_path"
export TROUPE_STRESS_BASELINE="$baseline_path"
export TROUPE_STRESS_BASELINE_RAW="$baseline_raw_path"
export TROUPE_STRESS_REVIEW="$review_path"
export TROUPE_STRESS_LEASE_ID="$lease_id_sha256"
export TROUPE_STRESS_EXCLUSIVE_STARTED_NS="$exclusive_started_ns"
export TROUPE_NPM_CACHE="$npm_cache"
export TROUPE_PLAYWRIGHT_CACHE="$browser_cache"

set +e
node "$repository_root/frontend/diagnostics/scripts/maintain.mjs" \
  --npm-cache "$npm_cache" \
  --typecheck \
  --browser tests/stress/diagnostics-stress.spec.ts \
  --browser-cache "$browser_cache" \
  --project chromium
maintainer_status=$?
set -e

verify_frozen_inputs || fail "the benchmark mutated a frozen baseline or review"
[[ "$maintainer_status" -eq 0 ]] || exit "$maintainer_status"
[[ -f "$report_path" && ! -L "$report_path" ]] || fail "current raw report was not published as a regular file"

integration_sha="$(git -C "$repository_root" rev-parse HEAD)"
actor_design_sha="$(sha256sum "$repository_root/docs/design/actor-agent-session.md" | cut -d ' ' -f 1)"
diagnostics_design_sha="$(sha256sum "$repository_root/docs/design/production-diagnostics.md" | cut -d ' ' -f 1)"
plan_sha="$(sha256sum "$repository_root/docs/plan/production-diagnostics-implementation-plan.md" | cut -d ' ' -f 1)"
validator_sha="$(sha256sum "$repository_root/docs/plan/verify_production_diagnostics_plan.py" | cut -d ' ' -f 1)"
review_record_sha="$(sha256sum "$repository_root/docs/plan/production-diagnostics-plan-review-record.md" | cut -d ' ' -f 1)"

python3 - "$report_path" "$repeat" "$integration_sha" "$initial_baseline_sha" \
  "$initial_baseline_raw_sha" "$initial_review_sha" "$lease_id_sha256" "$exclusive_started_ns" \
  "$actor_design_sha" "$diagnostics_design_sha" "$plan_sha" "$validator_sha" "$review_record_sha" <<'PY'
import json
import sys
from pathlib import Path

(
    report_name,
    repeat,
    integration_sha,
    baseline_sha,
    baseline_raw_sha,
    review_sha,
    lease_sha,
    started_ns,
    actor_design_sha,
    diagnostics_design_sha,
    plan_sha,
    validator_sha,
    review_record_sha,
) = sys.argv[1:]
report = json.loads(Path(report_name).read_text(encoding="utf-8"))
identity = report["identity"]
reference = report["reference"]
environment = report["environment"]
interval = report["exclusive_interval"]


def require(condition: bool, message: str) -> None:
    if not condition:
        raise SystemExit(f"diagnostics stress report validation failed: {message}")


require(report["schema"] == "troupe.diagnostics.performance-raw.v1", "schema")
require(report["kind"] == "gate", "report kind")
require(report["result"]["status"] == "passed", "result status")
require(report["result"]["violations"] == [], "result violations")
require(len(report["samples"]) == int(repeat), "sample count")
require(identity == {
    "actor_design_sha256": actor_design_sha,
    "diagnostics_design_sha256": diagnostics_design_sha,
    "plan_sha256": plan_sha,
    "validator_sha256": validator_sha,
    "review_record_sha256": review_record_sha,
    "integration_sha": integration_sha,
}, "identity")
require(reference == {
    "baseline_sha256": baseline_sha,
    "baseline_raw_sha256": baseline_raw_sha,
    "review_sha256": review_sha,
}, "frozen references")
require(
    environment["toolchain"]["chromium_version"]
    == environment["toolchain"]["chromium_expected_version"],
    "pinned Chromium version",
)
require(interval["resource"] == "benchmark-host", "exclusive resource")
require(interval["lease_id_sha256"] == lease_sha, "exclusive lease")
require(interval["started_at_epoch_ns"] == started_ns, "exclusive start")
require(int(interval["ended_at_epoch_ns"]) >= int(started_ns), "exclusive interval")
require(
    all(
        sample["browser_version"] == environment["toolchain"]["chromium_version"]
        for sample in report["samples"]
    ),
    "sample browser versions",
)
PY

printf 'diagnostics stress runner: passed (%s)\n' "$report_path"
