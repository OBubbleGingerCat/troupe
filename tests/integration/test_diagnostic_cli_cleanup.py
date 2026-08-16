from __future__ import annotations

import json
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
SOURCE = ROOT / "rust" / "src" / "application" / "diagnostic_cli" / "cleanup_apply.rs"
RUST_TEST = ROOT / "rust" / "tests" / "diagnostic_cli_cleanup_apply.rs"
ARTIFACT = ROOT / "tests" / "fixtures" / "artifact_layout" / "nodes" / "D11.json"
GATE = ROOT / "tests" / "fixtures" / "diagnostic_node_gates" / "D11.json"


def _read(path: Path) -> str:
    return path.read_text(encoding="utf-8")


def test_apply_revalidates_registry_process_store_and_exclusive_lease_per_run() -> None:
    source = _read(SOURCE)

    for required in (
        "fresh_cleanup_candidate",
        "discover_with(production, process_probe, server_probe)",
        "revalidate_for_cleanup",
        "validate_archive_before_lock",
        "ArchiveTarget::open_expected",
        "CleanupArchiveLease::acquire",
        "validated.revalidate(&source)",
        "BeforeExclusiveLease",
        "AfterExclusiveLease",
    ):
        assert required in source


def test_apply_uses_durable_atomic_undiscovery_before_no_follow_tree_removal() -> None:
    source = _read(SOURCE)

    for required in (
        "CLEANUP_INTENT_PREFIX",
        "CLEANUP_TOMBSTONE_PREFIX",
        "rename_noreplace(&source, &tombstone)",
        "RENAME_NOREPLACE",
        "TargetParentSync",
        "SourceParentSync",
        "remove_validated_directory",
        "archive symlink is rejected",
        "directory.file.sync_all()",
        "DeleteParentSync",
    ):
        assert required in source

    assert source.index("rename_noreplace(&source, &tombstone)") < source.index(
        "remove_validated_directory(&tombstone, validated.root)"
    )
    assert source.index("SourceParentSync") < source.index(
        "remove_validated_directory(&tombstone, validated.root)"
    )


def test_partial_failures_and_crash_intermediates_have_closed_stable_reporting() -> None:
    source = _read(SOURCE)

    for stable_code in (
        '"registry_cleanup_failed"',
        '"intent_failed"',
        '"rename_failed"',
        '"namespace_sync_failed"',
        '"delete_failed"',
        '"recovery_conflict"',
        '"recovery_invalid"',
        '"mutation_failed"',
        '"postcondition_failed"',
    ):
        assert stable_code in source

    for required in (
        "scan_tombstones",
        "parse_tombstone",
        "recover_tombstone",
        "RecoveredDeletion",
        "multiple cleanup tombstones exist for the same Run",
    ):
        assert required in source


def test_batch_postcondition_and_exact_result_do_not_reuse_preview_assumptions() -> None:
    source = _read(SOURCE)

    assert source.count("preview_with(") >= 2
    assert "preview.runs().iter().all(|run| !run.selected())" in source
    assert "policy_satisfied && !hard_failure" in source
    assert "CleanupApplyOperationFailure::PolicyUnsatisfied" in source
    assert "CleanupApplyOperationFailure::MutationFailed" in source


def test_rust_contract_covers_deletion_authority_races_recovery_and_run_isolation() -> None:
    source = _read(RUST_TEST)

    for test_name in (
        "exact_apply_deletes_complete_and_incomplete_whole_run_directories",
        "active_and_shared_reader_leases_are_never_deleted_by_exact_apply",
        "definite_stale_registry_is_revalidated_and_durably_removed_with_archive",
        "stale_owner_that_becomes_active_after_preview_is_revalidated_and_skipped",
        "store_identity_race_is_detected_after_exclusive_lease_upgrade",
        "symlink_race_is_rejected_without_following_or_touching_external_target",
        "rename_sync_failure_leaves_a_stable_tombstone_and_next_apply_recovers_it",
        "recovery_fails_closed_when_a_durable_intent_loses_its_lease_anchor",
        "batch_continues_other_runs_after_delete_failure_and_reports_unsatisfied",
        "exact_deletion_is_isolated_from_a_concurrent_run_and_reports_stable_json",
    ):
        assert f"fn {test_name}()" in source

    assert "std::env::temp_dir()" in source
    assert '"troupe-d11-{label}-{}-{sequence}"' in source
    assert "workspace" not in source.lower()


def test_d11_descriptors_are_realized_with_exact_direct_gate_and_no_build_extras() -> None:
    artifact = json.loads(ARTIFACT.read_text(encoding="utf-8"))
    gate = json.loads(GATE.read_text(encoding="utf-8"))

    assert artifact == {
        "state": "realized",
        "introduced": [
            "rust/tests/diagnostic_cli_cleanup_apply.rs",
            "tests/integration/test_diagnostic_cli_cleanup.py",
        ],
        "modified": ["rust/src/application/diagnostic_cli/cleanup_apply.rs"],
        "removed": [],
        "generated": [],
    }
    assert gate == {
        "state": "realized",
        "argv": [
            [
                "cargo",
                "test",
                "--locked",
                "--manifest-path",
                "rust/Cargo.toml",
                "--package",
                "troupe",
                "--test",
                "diagnostic_cli_cleanup_apply",
            ],
            ["pytest", "-q", "tests/integration/test_diagnostic_cli_cleanup.py"],
        ],
        "env": {},
        "maturin_features": [],
        "cache_requirements": [],
        "exclusive_resources": [],
    }
