# V05 Performance Baseline Review

Status: accepted for the production-diagnostics implementation gate.

Reviewer: Codex root implementation owner (self-review).

Independence: the plan's independent-review requirement is explicitly waived for
this implementation pass by the user's 2026-08-16 instruction to fix quickly and
use self-review instead of four independent votes. This record does not claim an
independent reviewer.

## Frozen Inputs

- Baseline SHA-256: `971cf1323159e370cf4aebd2592ece9bdf9505c1b507b7ff998c6d5cdd2fceec`
- Calibration raw SHA-256: `daafe9e1c54031229441b9fe946d5c35c54943d1db5188c825288b581d9e43d8`
- Calibration result SHA-256: `80ed0b0dd7d496a384c29920658898220623161874366355971d2e215b66540c`
- Samples: 3 serial runs under the exclusive `benchmark-host` lease
- Workload: 12,000 live events, 4,000 paused events, 10,000 visible primitives,
  2,000 indexed hit tests, and 100 rerenders in one frame

## Environment

- Linux 6.8.0-124-generic, x64, AMD EPYC 9654, 384 logical CPUs
- `schedutil` / `acpi-cpufreq`, 1,500,000-3,709,357 kHz, boost enabled
- Node 22.22.0, npm 10.9.4, Playwright 1.62.1
- Pinned Chromium 151.0.7922.34, revision 1234, ubuntu22.04-x64
- Package lock SHA-256: `d02077d88fce2afe6c62a2f6d5aa75b2a5bcfe6cbbda040f1cffbffe35eba595`
- npm cache manifest SHA-256: `342bda128372b4fd0caa81410078db66a08228de49ac1a81c6f63a62af0b6aea`
- Browser contract SHA-256: `19a225a7747d22b60fb56afcfe4ea9b25846058295fd57128b021cdba9e9b8c5`
- Browser cache manifest SHA-256: `f403a1698a959ee0ac7af4fba10717ddf949b8bac67ab05c6299373fb95e918b`

## Timing Review

| Metric | Calibration max (ms) | Variance ratio | Frozen max (ms) |
|---|---:|---:|---:|
| State reduction | 167.900 | 0.330 | 500 |
| Paused reduction | 114.900 | 0.087 | 300 |
| Timeline layout | 14.100 | 0.216 | 50 |
| Timeline draw | 14.400 | 0.345 | 50 |
| Indexed hit test | 2.700 | 0.180 | 20 |
| Batched rAF updates | 13.800 | 0.163 | 75 |

The common run-to-run variance ceiling is 0.60 and the frozen timing-noise floor
is 5 ms. The floor prevents sub-millisecond timer and scheduler noise from
dominating ratios for the 2-3 ms hit-test batch. The ceiling remains above the
observed 0.345 maximum while detecting a sustained multi-run regression. Duration
limits retain roughly 2.6x-7.4x calibration headroom; they are intentionally not
machine-score targets and cannot be widened by the gate.

## Heap And Render Review

- Peak heap: 5,765,664 bytes observed; 16,777,216 bytes frozen maximum.
- Retained heap: 3,388,872 bytes observed; 8,388,608 bytes frozen maximum.
- Recovery ratio: 0.819937 observed; 0.90 frozen maximum.
- Every sample retained the bounded reducer windows and LRU capacities.
- Every sample drew 10,000 primitives, examined at most one indexed primitive per
  hit, and performed exactly one Canvas draw for the 100-rerender batch.
- Selection, span pairing, usage coverage, gap visibility, pause freezing, and
  query-based resume invariants passed in all samples.

The heap limits allow runtime/JIT retention measured by Chromium but reject a
roughly 2.5x retained-set increase or a missing recovery boundary. The exact raw
samples and environment fingerprint remain the authority for future threshold
changes; changing a threshold requires a new calibration raw file and review.
