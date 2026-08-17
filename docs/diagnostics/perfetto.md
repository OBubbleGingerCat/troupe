# Perfetto trace export

`troupe diagnostic dump` produces a native TrackEvent `.pftrace` for offline
inspection. Troupe does not use Perfetto for the real-time Web interface and
does not embed Perfetto UI. Open the resulting local file manually with the
public Perfetto UI or another compatible trace processor. Dumping requires no
user installation of Node, npm, Perfetto source, SDK, or protobuf compiler.

## Capture paths

All paths export a stable committed prefix. At command admission the reader
captures committed watermark `W`; the default prefix is `1..W`, while
`--through SEQ` selects `1..SEQ` and must not exceed `W`. An empty prefix is a
valid trace containing Run descriptors and metadata but no event packets.

For a local inactive archive, the T03 encoder reads through a shared archive
lease and streams the prefix to the T08 local atomic-file publisher. A
`--production` target that resolves to an inactive archive follows the same
path.

For `--url`, or `--production` resolved to an active server, the CLI calls the
identity-checked, read-only `GET /api/v1/dump[?through=SEQ]` endpoint. The server
captures and encodes the prefix, streams it to the caller, and the caller's T08
publisher creates the requested local output. The HTTP request cannot name a
server filesystem path or request server-side force replacement. CLI
`--output` and `--force` affect only the caller's local filesystem.

An active request borrows the Runtime's already-held active guard and opens a
read transaction; it never reacquires an archive lock. An archive request owns
a shared lease for the request and releases it on completion or disconnect.
Neither path writes a trace on the server, changes the canonical store, uploads
the trace, or opens a public service automatically.

## Atomic publication

The local T08 wrapper creates an exclusive temporary regular file in the output
directory, encodes, flushes, syncs, and closes it, and then performs same-directory
atomic publication. Without `--force`, an existing target is never replaced.
Force accepts only an identity-checked regular file, creates an exclusive hard-link
backup of its old inode, and durably syncs that backup before replacement.
Directories, symlinks, special files, traversal, and cross-directory targets
are rejected.

Every report has exactly one publication state:

- `published`: the new target and removal of temporary/backup names are durably
  proven.
- `not_published`: publication did not commit, or an identity-checked rollback
  restored the prior target or absence and durably synced the directory.
- `publication_indeterminate`: a post-rename sync, rollback, identity check, or
  cleanup sync failed, so the final namespace cannot be proven.

The indeterminate state reports a stable phase and observed target, temporary,
and backup identities. Preserve every discoverable path and inspect them
manually. Do not automatically retry, delete residue, or claim that the old
target is unchanged. A retry could overwrite the only recoverable inode.

## Trace meaning and precision

Actor is a logical track group; Scene, Cue, mailbox, Act caller and turn, tool,
and lifecycle work become slices. Dispatch, Effect return, cancellation, and
handoff become flows. Context, queue, active-turn, rejection, and known usage
aggregates become counters. Gaps and custom instants remain markers. Open spans
have no invented end, and overlapping non-LIFO spans receive deterministic
sibling lanes.

Every timestamped packet uses Perfetto built-in trace-file clock ID `11` and the
canonical Run-relative `elapsed_ns`. No wall-clock snapshot is synthesized.
Elapsed values above signed 64-bit nanoseconds fail export rather than wrapping,
clamping, rescaling, or changing clocks.

A counter is emitted only when the canonical value is an exact `int64` or an
exact finite double. Any other integer or decimal remains canonical decimal text
on its scope timeline with `troupe.counter_projection="not_exact"`; it is never
rounded into a misleading counter. Missing usage remains absent rather than
zero. Agent message identity, byte counts, and completion are represented, but
message body text is not exported.

The trace metadata binds exporter and event schema versions, full Run ID,
captured watermark, exported-through watermark, Troupe version, Production
outcome and clean-shutdown availability, and this warning:
`trace may contain sensitive diagnostic metadata and user-provided attributes`.
Treat the file as sensitive even though its content policy is narrower than the
canonical store and Web transcript.

## Compatibility provenance

The private protobuf declarations are audited against official Perfetto v57.2,
commit `da1d152cff27890903d158fe96751de3aab883cc`, with checked upstream source
hashes and a closed used-field manifest. Release compatibility has three pinned
offline layers: independent protobuf decoding of byte-exact goldens, official
v57.2 `trace_processor_shell` SQL assertions, and a pinned official Perfetto UI
browser smoke test. A current public UI check is informational and does not
define release correctness.
