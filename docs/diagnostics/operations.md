# Diagnostic operations

Production diagnostics are a mandatory Runtime subsystem. Each Production Run
owns an in-process HTTP server, a canonical event pipeline, and a persistent
SQLite store. There is no disable switch, alternate state root, memory-only
fallback, or best-effort mode.

## Startup and discovery

The Production package root must be writable. Before importing or constructing
Production code, Troupe creates and probes the state tree, initializes and
commits the store, binds the listener, and durably publishes the registry
entry. Any failure in that sequence is a startup failure, so no user code runs.

The fixed layout is:

```text
<production-root>/.troupe/
`-- diagnostics/
    |-- instances/
    |   `-- <run-id>.json
    `-- runs/
        `-- <run-id>/
            |-- diagnostics.sqlite3
            |-- diagnostics.sqlite3-wal   (while present)
            `-- diagnostics.sqlite3-shm   (while present)
```

Troupe-owned directories use exact owner-only mode `0700`; locators, temporary
locators, the database, and SQLite sidecars use `0600`. Troupe verifies and, when
safe, tightens modes instead of relying on the process umask. A permission or
mode-verification failure is a core failure.

Each immutable `instances/<run-id>.json` locator records the full Run ID,
process identity, bind address, actual port, loopback-capable local endpoint,
optional explicit advertise URL, archive directory, start time, protocol
versions, and `security_scope="trusted_network"`. It is published only after
the store and listener are ready. Multiple active Runs have separate files;
there is no singleton `active.json` or implicit latest Run.

After durable registry publication and before Production import, stderr gets
exactly one line with this shape:

```text
troupe: diagnostic ready {"locator_schema_version":1,"run_id":"...","local_url":"http://127.0.0.1:43120","advertise_url":null,"archive_directory":"/abs/production/.troupe/diagnostics/runs/...","security_scope":"trusted_network"}
```

The JSON locator is the startup-ready signal. A registry file can remain after
a hard crash, so clients revalidate owner process identity and server Run
identity before connecting. Only a revalidated definitely-stale entry is
eligible for automatic registry cleanup; unhealthy, invalid, incompatible, or
identity-mismatched entries are retained for inspection.

## Network boundary

The default listener is `0.0.0.0` with port `0`, which asks the OS to allocate
an available port. `0.0.0.0` is a bind address, not a client URL. The locator
provides a local URL, and an operator may explicitly configure an HTTP or HTTPS
advertise URL for a reachable LAN or reverse-proxy endpoint.

The direct listener is plain HTTP and has no authentication, authorization,
login, session, or credential mechanism. It is supported only on a trusted
LAN. Every network peer able to connect can read captured messages, metadata,
usage, and business annotations. Use an external VPN, SSH tunnel, or
TLS-terminating reverse proxy when stronger transport protection is required.

The UI, API, and event stream share one origin. The server does not opt another
browser origin into reading responses. It ignores every case and repetition of
`Forwarded`, `X-Forwarded-Host`, `X-Forwarded-Proto`, and
`X-Forwarded-Prefix`; these headers cannot alter identity, routes, or the public
URL. Configure the advertise URL explicitly behind a proxy.

All endpoints are read-only. Troupe validates data shape and size but does not
inspect semantic content, find credentials, or redact captured values. Network
placement, capture choices, retention, and exported files are operator
responsibilities.

## Runtime and shutdown failures

The diagnostic server runs in the Production process under the Runtime
supervisor. A server execution-context exit, unexpected listener close,
mandatory ingress exhaustion, writer stall, transaction or commit error, disk
or permission failure, store invariant failure, or configured Run quota
crossing is fatal. Troupe stops admitting new Production and Cue work, performs
bounded settlement and diagnostic drain, and exits non-zero. It never discards
core facts and continues the agent flow as if observation were healthy.

A single invalid request, disconnected or slow client, archive query, optional
Python sink callback, or requested export is isolated to that operation. These
consumers use separate bounded delivery and do not backpressure the mandatory
writer.

Normal shutdown commits terminal facts, outcome, final watermarks, and
`clean_shutdown=true`; closes live streams; durably removes this Run's instance
locator; and then closes listener, readers, writer, store, and active lease. A
core persistence, registry, or server shutdown failure changes the process to a
non-zero exit. If finalization did not commit, the archive remains explicitly
incomplete with `clean_shutdown=false`.

## Archives, leases, quota, and retention

An active Run has a validated live registry owner and an exclusive active
archive lease. A completed archive has durably finalized diagnostics and
`clean_shutdown=true`; its business outcome can still be completed, failed, or
cancelled. An incomplete archive has `clean_shutdown=false` and may contain only
the dense committed prefix recovered after a diagnostic failure or hard crash.

Active HTTP readers reuse the Runtime-held guard. Inactive archive status,
snapshot, event, dump, and temporary archive-server readers acquire shared
leases. Cleanup requires an exclusive cleanup lease. Consequently cleanup cannot race
an active Run or an archive reader, and an active-store client cannot bypass the
live server by opening SQLite directly.

By default, completed and incomplete archives are retained indefinitely, and
an active Run never trims its early events. The optional per-Run byte quota is
unset by default. When configured, it counts validated regular files for the
whole Run directory, including SQLite sidecars; reaching the fail-closed budget
terminates the Production rather than deleting history.

Archive cleanup is explicit and defaults to a preview. Policies can select one
Run, an age cutoff, a number of newest clean archives to keep, or a total byte
budget. Batch policies automatically select only cleanly finalized archives;
an incomplete archive requires exact selection. Applying cleanup revalidates
identity and takes the exclusive lease, atomically removes the entire Run
directory from the discoverable namespace, and then deletes its validated
contents without following symlinks. It never removes individual event rows,
the database alone, or SQLite sidecars alone.

## Checked status example

The following output is checked byte-for-byte against the repository's human
status fixture. A failed or incomplete observed Production is still a
successful status query; lifecycle state is data, not a command failure.

<!-- BEGIN DIAGNOSTIC STATUS FIXTURE -->
```text
api_schema_version: 1
run_id: 12345678-1234-4234-9234-123456789abc
source: archive
security_scope: trusted_network
store_schema_version: 1
store_schema_identity: troupe.diagnostics.store.v1
event_schema_version: 1
configuration_identity: configuration-sha256:d02
event_watermark: 0
read_model_watermark: 0
lifecycle:
  state: incomplete
  started_at: 2026-08-16T00:00:00Z
  ended_at: null
  outcome: null
  clean_shutdown: false
writer:
  status: unavailable
  reason: archive
quota:
  status: unavailable
  reason: archive
```
<!-- END DIAGNOSTIC STATUS FIXTURE -->
