from __future__ import annotations

from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
CUSTOM_ACT = ROOT / "rust/src/diagnostic_runtime/custom_act_binding.rs"
CUSTOM_RUNTIME = ROOT / "rust/src/diagnostic_runtime/custom_binding.rs"
SINK_BINDING = ROOT / "rust/src/diagnostic_runtime/sink_binding.rs"
SINK_PROJECTION = ROOT / "rust/src/diagnostic_runtime/sink_projection.rs"
PYTHON_TASK = ROOT / "rust/src/orchestration/python_task.rs"
SCENE_CONTEXT = ROOT / "rust/src/orchestration/scene_context.rs"


def _source(path: Path) -> str:
    return path.read_text(encoding="utf-8")


def _between(source: str, start: str, end: str) -> str:
    start_index = source.index(start)
    return source[start_index : source.index(end, start_index)]


def test_act_authority_is_exact_generation_bound_and_never_ambient() -> None:
    authority = _source(CUSTOM_ACT)
    custom = _source(CUSTOM_RUNTIME)

    roles = _between(
        authority,
        "pub(crate) enum ActAuthorityRole",
        "pub(crate) struct ActTaskAuthority",
    )
    assert roles.count("Caller,") == 1
    assert roles.count("CallerDescendant,") == 1
    assert roles.count("Supervisor,") == 1
    for field in (
        "binding: Weak<RunBinding>",
        "act_id: RunLocalId",
        "generation: u64",
        "caller_active: bool",
        "generation_active: bool",
    ):
        assert field in authority

    resolve = _between(authority, "    fn resolve(\n", "    pub(crate) fn observe(")
    assert "generation != self.inner.generation" in resolve
    assert "!phase.generation_active" in resolve
    assert "caller_role && !phase.caller_active" in resolve
    assert "Arc::ptr_eq(&expected_binding, binding)" in resolve
    assert "act_producer::lineage_snapshot(self.inner.act_id.as_str())" in resolve
    assert "snapshot.act_scope() != &self.inner.act_scope" in resolve
    assert "snapshot.event_scope().clone()" in resolve
    assert "bound_sink_for" not in authority
    assert "cue_producer" not in authority

    extension = _between(custom, "fn snapshot_from_lineage(", "fn current_task")
    assert extension.index("domain_extension()") < extension.index("lineage.runtime()")
    assert extension.index("domain_extension()") < extension.index("lineage.cued()")
    domain = _between(authority, "impl CustomDomainExtension", "fn ensure_domain_extension")
    assert ".act_authority()" in domain
    assert ".transpose()" in domain


def test_only_registered_tasks_propagate_act_authority() -> None:
    authority = _source(CUSTOM_ACT)
    task = _source(PYTHON_TASK)
    scene = _source(SCENE_CONTEXT)

    child = _between(
        authority,
        "pub(crate) fn for_registered_child",
        "pub(crate) fn is_supervisor",
    )
    assert "ActAuthorityRole::CallerDescendant" in child
    assert "ActAuthorityRole::Supervisor => ActAuthorityRole::Supervisor" in child
    assert "act_authority: Option<ActTaskAuthority>" in task
    assert "map(ActTaskAuthority::for_registered_child)" in task

    delegated = _between(
        scene,
        "pub(crate) fn create_delegated_task(",
        "pub(crate) fn new_for_test(py: Python<'_>)",
    )
    assert delegated.index("consume_exact(coroutine)") < delegated.index(
        "current_lineage(py)?"
    )
    assert ".map(|lineage| lineage.for_registered_child())" in delegated
    assert "Self::base_task_owns_coroutine" in delegated
    assert "self.register_task(&task, lineage)?" in delegated

    current = _between(scene, "pub(crate) fn current_lineage", "validate_lineage_for_scene")
    for guard in (
        "self.pid != std::process::id()",
        "self.thread_id != std::thread::current().id()",
        "current_task_is_canonical",
        "running_loop_is_canonical",
        "running_loop.is(self.event_loop.bind(py))",
    ):
        assert guard in current
    assert "ActTaskAuthority::active_supervisor" in task
    assert "ActTaskAuthority::is_supervisor" in task


def test_caller_revocation_and_terminal_expiry_use_one_ordered_subscriber() -> None:
    authority = _source(CUSTOM_ACT)
    sink = _source(SINK_BINDING)

    observer = _between(authority, "pub(crate) fn observe", "fn update_caller_lineage")
    assert "SpanKind::ActCaller" in observer
    assert "caller_active = false" in observer

    delivery = _between(
        sink,
        "impl ActEventSubscriber for ActSinkSubscriber",
        "struct PayloadState",
    )
    assert delivery.index("authority.observe(&event)") < delivery.index(
        "self.deliver_projected(event)"
    )
    projected = _between(sink, "fn deliver_projected(", "fn settle_terminal(")
    assert projected.index("try_enqueue_terminal") < projected.index("settle_terminal")

    expiry = _between(
        authority,
        "impl ActAuthorityExpiry for ActAuthority",
        "struct ActDomainExtension",
    )
    assert expiry.index("phase.generation_active = false") < expiry.index(
        "caller_base_lineage.clone()"
    )
    assert "*lock(&self.authority.inner.phase) = previous" in expiry
    assert "with_act_authority(self.authority.token(ActAuthorityRole::Caller))" in expiry

    authority_only = _between(
        sink,
        "struct AuthorityOnlySubscriber",
        "struct ActSinkSubscriber",
    )
    assert "event.built_in_span_kind() != Some(SpanKind::ActLifecycle)" in authority_only
    assert authority_only.index("settle_authority_only()") < authority_only.index(
        "retire_authority_expected"
    )


def test_custom_sink_projection_reuses_the_canonical_fact_and_frozen_capture() -> None:
    projection = _source(SINK_PROJECTION)
    sink = _source(SINK_BINDING)

    selected = _between(projection, "fn event_selected(", "pub(crate) const fn span_selected")
    assert "DiagnosticEvent::CustomSpanStarted(_)" in selected
    assert "DiagnosticEvent::CustomSpanFinished(_)" in selected
    assert "DiagnosticEvent::CustomInstantOccurred(_)" in selected
    assert "DiagnosticEvent::CustomCounterSampled(_) => capture.custom_events" in selected

    projector = _between(projection, "pub(crate) fn project_act_event", "fn event_impacts")
    assert "canonical: canonical.clone()" in projector
    assert "same_act" not in sink
    assert "emit_custom_" not in sink
    assert "project_act_event(&canonical, &self.act_scope, self.capture" in sink
