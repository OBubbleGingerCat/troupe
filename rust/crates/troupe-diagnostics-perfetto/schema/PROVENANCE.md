# Perfetto schema provenance

`src/schema.rs` is a hand-maintained subset derived from Perfetto v57.2 at
commit `da1d152cff27890903d158fe96751de3aab883cc`.

Source repository: `https://github.com/google/perfetto`

Only fields used by the Troupe encoder are mirrored. The source schema is
licensed under Apache License 2.0; a copy is retained in `upstream/LICENSE`.
The empty selected `CounterDescriptor` is intentional: its presence marks a
track as a counter track without selecting optional unit or category fields.
