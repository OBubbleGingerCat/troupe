from __future__ import annotations

import json
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
SHUTDOWN = ROOT / "rust/src/diagnostic_runtime/shutdown.rs"
WRITER = ROOT / "rust/crates/troupe-diagnostics-runtime/src/store/writer.rs"
BOOTSTRAP = ROOT / "rust/src/diagnostic_runtime/bootstrap.rs"
ACTIVATION = ROOT / "rust/src/diagnostic_runtime/activation.rs"
PRODUCER = ROOT / "rust/src/diagnostic_runtime/runtime_producer.rs"
CLI = ROOT / "rust/src/application/cli.rs"
MATRIX = ROOT / "tests/fixtures/diagnostics/shutdown-phase-matrix.json"
ARTIFACT = ROOT / "tests/fixtures/artifact_layout/nodes/X02.json"
GATE = ROOT / "tests/fixtures/diagnostic_node_gates/X02.json"


def _read(path: Path) -> str:
    return path.read_text(encoding="utf-8")


def _json(path: Path) -> dict[str, object]:
    value = json.loads(_read(path))
    assert isinstance(value, dict)
    return value


def test_shutdown_matrix_closes_order_and_failure_contract() -> None:
    matrix = _json(MATRIX)

    assert matrix["schema_version"] == 1
    assert matrix["preconditions"] == [
        "stop_new_work",
        "settle_or_cancel_current_work",
        "persist_runtime_sink_usage_and_lifecycle_terminal_facts",
    ]
    assert matrix["owned_phases"] == [
        "seal_canonical_ingress",
        "bounded_writer_drain",
        "final_metadata_transaction",
        "best_effort_stream_closed",
        "durable_registry_unpublish",
        "close_listener_and_readers",
        "close_writer_and_sqlite",
        "release_active_lease_and_thread",
    ]
    assert matrix["final_transaction"] == {
        "fields": ["ended_at", "production_outcome", "clean_shutdown"],
        "clean_shutdown": True,
        "preserve_equal_watermarks": True,
        "outcomes": ["completed", "failed", "cancelled"],
        "single_transition": True,
    }
    assert matrix["failure_contract"] == {
        "final_transaction_failure_keeps_incomplete": True,
        "core_failure_uses_abort_path": True,
        "later_close_failure_exits_nonzero": True,
        "peer_disconnect_does_not_downgrade_archive": True,
        "always_attempt_remaining_cleanup": True,
    }


def test_writer_final_transaction_is_atomic_single_transition() -> None:
    writer = _read(WRITER)
    finalization = writer[writer.index("pub fn finalize_run_with_hook") :]

    assert "WriteStatement::FinalizeRunMetadata" in finalization
    assert "SET ended_at = ?1, production_outcome = ?2" in finalization
    assert "committed_key = ?3, committed_sequence = ?4" in finalization
    assert "read_model_key = ?3" in finalization
    assert "read_model_sequence = ?4" in finalization
    assert "clean_shutdown = 1" in finalization
    assert "ended_at IS NULL" in finalization
    assert "production_outcome IS NULL" in finalization
    assert "clean_shutdown = 0" in finalization
    assert finalization.index("hook.before_commit") < finalization.index(
        "transaction.commit()"
    )
    assert finalization.index("transaction.commit()") < finalization.index(
        "refresh_metadata_after_commit"
    )
    assert "FinalStateConflict" in finalization
    assert "final_statement_failure_rolls_back_and_leaves_archive_incomplete" in writer


def test_runtime_uses_two_phase_writer_and_closed_resource_order() -> None:
    bootstrap = _read(BOOTSTRAP)
    shutdown = _read(SHUTDOWN)
    coordinator = shutdown[shutdown.index("pub(crate) fn run_ordered_shutdown") :]

    for command in ("Finalize", "Abort", "Close"):
        assert command in bootstrap[bootstrap.index("enum WriterCommand") :]
    assert "supervisor.begin_shutdown" in bootstrap
    assert "wait_for_writer_close" in bootstrap
    assert "checkpoint_and_close_writer" in bootstrap
    assert coordinator.index("resources.seal_ingress()") < coordinator.index(
        "resources.finalize_writer"
    )
    assert coordinator.index("resources.close_live_stream") < coordinator.index(
        "resources.unpublish_registry"
    )
    assert coordinator.index("resources.unpublish_registry") < coordinator.index(
        "resources.close_listener_and_readers"
    )
    assert coordinator.index("resources.close_listener_and_readers") < coordinator.index(
        "resources.close_writer_and_store"
    )
    assert coordinator.index("resources.close_writer_and_store") < coordinator.index(
        "resources.release_runtime_resources"
    )


def test_activation_retains_terminal_producer_and_stream_signal() -> None:
    activation = _read(ACTIVATION)
    producer = _read(PRODUCER)
    cli = _read(CLI)

    assert "producer: Arc<OnceLock<Arc<runtime_producer::RuntimeLifecycleProducer>>>" in activation
    assert "Arc::clone(&self.producer)" in activation
    assert "producer.terminal_outcome()" in activation
    assert "pub(crate) fn terminal_outcome" in producer
    assert "if !state.run_closed" in producer
    assert "impl FinalStreamCloser for DeferredCommitObserver" in activation
    assert ".with_final_stream_closer(Box::new(final_commits))" in activation
    assert "shutdown_clean_or_abort" in activation
    assert "finalize_user_failure" in activation
    assert "shutdown_ordered()" in cli
    assert '"troupe: diagnostic shutdown failed: "' in activation
    assert "error.shutdown_line()" in cli


def test_x02_descriptors_are_realized_with_one_source_gate() -> None:
    assert _json(ARTIFACT) == {
        "state": "realized",
        "introduced": [
            "tests/fixtures/diagnostics/shutdown-phase-matrix.json",
            "tests/integration/test_diagnostic_shutdown.py",
        ],
        "modified": [
            "rust/crates/troupe-diagnostics-runtime/src/store/writer.rs",
            "rust/src/application/cli.rs",
            "rust/src/diagnostic_runtime/activation.rs",
            "rust/src/diagnostic_runtime/bootstrap.rs",
            "rust/src/diagnostic_runtime/runtime_producer.rs",
            "rust/src/diagnostic_runtime/shutdown.rs",
        ],
        "removed": [],
        "generated": [],
    }
    assert _json(GATE) == {
        "state": "realized",
        "argv": [["pytest", "-q", "tests/integration/test_diagnostic_shutdown.py"]],
        "env": {},
        "maturin_features": [],
        "cache_requirements": [],
        "exclusive_resources": [],
    }
