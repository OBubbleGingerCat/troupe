# Perfetto schema provenance

The files under `upstream/` are unmodified snapshots from Perfetto v57.2 at
commit `da1d152cff27890903d158fe96751de3aab883cc`.

Source repository: `https://github.com/google/perfetto`

Only the closed field set in `used-fields.json` is mirrored by `src/schema.rs`.
The snapshots are audit inputs and license evidence; they are never compiled.
Imports needed only by unselected fields are intentionally not vendored.
The empty selected `CounterDescriptor` is intentional: its presence marks a
track as a counter track without selecting optional unit or category fields.

Run `python scripts/audit_perfetto_schema.py --offline` to verify the commit,
snapshot set, hashes, selected definitions, recursive type closure, and Rust
wire tags without invoking `protoc` or accessing the network.
