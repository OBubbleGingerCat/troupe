# Diagnostic CLI

`troupe diagnostic` inspects active and archived Runs without importing the
Production package, executing its modules, or constructing `Production`.
`--production` identifies the directory containing `.troupe`; it is not a
request to run that package.

## Command grammar

The V1 command surface is:

```console
troupe diagnostic runs --production PROD [--format human|json]

troupe diagnostic status TARGET [--format human|json]
troupe diagnostic snapshot TARGET [--format human|json]
troupe diagnostic events TARGET [--tail N | --after SEQ] [--follow] [--format human|jsonl]
troupe diagnostic dump TARGET --output FILE [--through SEQ] [--force]

troupe diagnostic serve (--production PROD --run RUN_ID | --archive RUN_DIRECTORY) [--port PORT] [--open]

troupe diagnostic cleanup --production PROD (--run RUN_ID | --older-than DURATION | --keep-runs N | --max-total-bytes SIZE) [--apply] [--format human|json]
```

`TARGET` means exactly one of these mutually exclusive selectors:

```console
--production PROD [--run RUN_ID]
--url BASE_URL
--archive RUN_DIRECTORY
```

`--run` requires `--production`. A production target selects an identity-checked
active server when present and otherwise an inactive archive; ambiguous Runs
must be selected explicitly. A URL is an absolute HTTP(S) base URL with no
query, fragment, or userinfo and is checked against server identity. An archive
is the complete Run directory, not a bare `diagnostics.sqlite3` file, and may be
a read-only copy whose basename is not its Run ID.

`runs` accepts only `--production`. It lists active, stale, unhealthy, unsafe,
invalid, incompatible, completed, and incomplete candidates without choosing an
implicit latest Run.

## Defaults and output

`runs`, `status`, and `snapshot` default to `--format human` and also support one
newline-terminated versioned JSON document. `events` defaults to human and
supports canonical JSONL with exactly one unwrapped `DiagnosticEvent` per line.
Machine stdout contains only requested data; progress, reconnect notices,
warnings, and errors go to stderr.

With neither start option, `events` means `--tail 100`. `--tail` and `--after`
are mutually exclusive; `--after 0` reads from the retained beginning.
`--follow` replays the selected range and follows committed live events with
sequence deduplication. It is invalid for an archive target. `--tail 0
--follow` starts after the committed head captured when the connection begins.

`dump` requires a filesystem `--output`; stdout is not a trace destination.
Without `--through`, it captures the committed head `W`; otherwise it exports
the exact prefix through the canonical sequence requested. Existing output is
not replaced unless `--force` is present.

Exit status is closed and scriptable:

| Status | Meaning |
|---:|---|
| `0` | Command completed successfully, including a successful observation of a failed or incomplete Production |
| `1` | Discovery, server, protocol, store, export, or cleanup operation failed |
| `2` | Command-line usage or argument validation failed |
| `130` | The user interrupted the command |

## Archive serving

`serve` requires an explicitly inactive target: either `--production PROD
--run RUN_ID` or `--archive RUN_DIRECTORY`. It does not accept `--url`. The
temporary server binds loopback only, defaults to OS-assigned `--port 0`, runs
in the foreground, writes a versioned `troupe: diagnostic archive ready {...}`
locator to stderr, and acquires a shared archive lease. It never modifies the
store or publishes an active instance locator. `--open` alone asks the system
browser to open the page; a browser-launch warning does not stop an already
ready server.

## Cleanup

`cleanup` requires exactly one of `--run`, `--older-than`, `--keep-runs`, or
`--max-total-bytes`. It is a preview unless `--apply` is explicit. Age, count,
and byte-budget policies select only cleanly finalized archives; an incomplete
archive requires exact `--run` selection. Preview and apply report protected,
leased, raced, skipped, selected, and unsatisfied candidates in human or
versioned JSON output.

Apply revalidates each candidate and requires an exclusive cleanup lease. It
removes a complete validated Run directory as one unit and never follows a
symlink or deletes only the database, event rows, WAL, or SHM file.

## Frozen command list

This excerpt is checked against the command help fixture:

<!-- BEGIN DIAGNOSTIC HELP FIXTURE -->
```text
Inspect active and archived Production diagnostics

Usage: troupe diagnostic <COMMAND>

Commands:
  runs      List active, stale, and archived Runs
  status    Show diagnostic and Production status
  snapshot  Read the current diagnostic snapshot
  events    Read or follow canonical diagnostic events
  dump      Export a captured prefix as a Perfetto trace
  serve     Serve an inactive archive on loopback
  cleanup   Preview or apply archive retention cleanup

Options:
  -h, --help  Print help
```
<!-- END DIAGNOSTIC HELP FIXTURE -->
