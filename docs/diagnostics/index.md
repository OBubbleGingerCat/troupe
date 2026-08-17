# Production diagnostics

Production diagnostics are mandatory, persistent, and read-only observation of
one Troupe Run. Start with the page matching the task at hand:

- [Operations and archives](operations.md): startup, registry, trusted-network
  deployment, persistent state, failures, leases, quota, retention, and cleanup.
- [Canonical events](events.md): event envelopes, ordering, spans, scopes,
  observation gaps, and completeness.
- [Python API](python.md): per-Act `DiagnosticSink`, capture policy, custom
  instrumentation, token usage, and static `ViewSpec` declarations.
- [Live Web interface](web.md): the Actor/Cue execution hierarchy, timeline,
  transcript, usage, declared Views, replay, reconnect, and bounded browser state.
- [Diagnostic CLI](cli.md): exact commands, targets, formats, exit status,
  archive serving, and cleanup.
- [Perfetto export](perfetto.md): stable-prefix `.pftrace` capture, local atomic
  publication, precision, sensitivity, and compatibility provenance.
- [Release checklist](RELEASE_CHECKLIST.md): executable release gates and
  retained acceptance evidence.
