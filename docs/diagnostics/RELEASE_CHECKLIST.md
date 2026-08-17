# Production Diagnostics Release Checklist

This checklist is defined by executable gates and retained evidence. A release passes only when the final runner exits successfully and publishes the acceptance record without replacing an existing record.

## Required Inputs

- A clean integration commit containing all 145 realized node fragments and Gate descriptors.
- Read-only npm, Playwright, and Perfetto caches matching their checked identity manifests.
- A fresh canonical `TROUPE_FINAL_ATTEMPT_ID` and a new attempt directory below `TROUPE_DIAGNOSTICS_EVIDENCE`.
- `INTEGRATION_SHA`, `PLAN_BUNDLE_SHA`, and `PRODUCT_BASE_SHA` bound to the accepted integration and planning history.

## Traceability Gate

```bash
scripts/run_diagnostic_bootstrap_gate.sh V11
```

This Gate runs the D1-D54 verifier against the tracked design, plan, ownership ledger, fragment index, and Gate descriptor catalog.

## Final Runner

```bash
scripts/test_diagnostics_final.sh --all --npm-cache "${TROUPE_NPM_CACHE:?}" --perfetto-cache "${TROUPE_PERFETTO_CACHE:?}" --browser-cache "${TROUPE_PLAYWRIGHT_CACHE:?}" --evidence-root "${TROUPE_DIAGNOSTICS_EVIDENCE:?}/attempts/${TROUPE_FINAL_ATTEMPT_ID:?}" --acceptance-path "${TROUPE_DIAGNOSTICS_EVIDENCE:?}/accepted.json" --attempt-id "${TROUPE_FINAL_ATTEMPT_ID:?}" --integration-sha "${INTEGRATION_SHA:?}"
```

The runner owns command ordering and preserves the first failure. A failed attempt is retained and any retry uses a new attempt ID and directory.

## Required Evidence

The attempt directory must contain the following create-new reports, each bound to the same integration commit, planning hashes, cache identities, attempt ID, and child result hashes:

- `V05-performance-raw.json`
- `V07-wheel-report.json`
- `V03-final-evidence.json`

Successful publication creates exactly one `${TROUPE_DIAGNOSTICS_EVIDENCE}/accepted.json`. The accepted record must validate against the checked-in accepted-evidence schema and bind the three report hashes. Missing, mismatched, overwritten, partially published, or indeterminate evidence is a failed release.
