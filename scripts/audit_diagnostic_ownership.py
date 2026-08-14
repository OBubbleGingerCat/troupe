#!/usr/bin/env python3
from __future__ import annotations

import argparse
import hashlib
import subprocess
import sys
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
SUPPORT = ROOT / "tests" / "support"
sys.path.insert(0, str(SUPPORT))

from artifact_layout import (  # noqa: E402
    ArtifactLayoutError,
    load_artifact_layout,
    load_gate_descriptors,
)


class OwnershipAuditError(RuntimeError):
    pass


def _git(*arguments: str) -> str:
    try:
        completed = subprocess.run(
            ["git", *arguments],
            cwd=ROOT,
            check=True,
            capture_output=True,
            text=True,
        )
    except subprocess.CalledProcessError as error:
        detail = error.stderr.strip() or error.stdout.strip() or str(error)
        raise OwnershipAuditError(detail) from error
    return completed.stdout


def _resolve_commit(value: str) -> str:
    resolved = _git("rev-parse", "--verify", f"{value}^{{commit}}").strip()
    if not resolved:
        raise OwnershipAuditError(f"could not resolve base commit: {value}")
    if subprocess.run(
        ["git", "merge-base", "--is-ancestor", resolved, "HEAD"],
        cwd=ROOT,
        check=False,
    ).returncode != 0:
        raise OwnershipAuditError("ownership audit base is not an ancestor of HEAD")
    return resolved


def _changed_paths(base: str) -> dict[str, str]:
    result: dict[str, str] = {}
    for line in _git("diff", "--name-status", "--no-renames", base, "HEAD", "--").splitlines():
        try:
            status, path = line.split("\t", 1)
        except ValueError as error:
            raise OwnershipAuditError(f"malformed git diff entry: {line!r}") from error
        if status not in {"A", "M", "D"}:
            raise OwnershipAuditError(f"unsupported git diff status {status!r} for {path}")
        if path in result:
            raise OwnershipAuditError(f"duplicate git diff path: {path}")
        result[path] = status
    return result


def audit_node(node_id: str, base: str) -> None:
    try:
        layout = load_artifact_layout(ROOT)
        gates = load_gate_descriptors(ROOT)
    except ArtifactLayoutError as error:
        raise OwnershipAuditError(str(error)) from error
    if node_id not in layout.fragments:
        raise OwnershipAuditError(f"unknown diagnostic node: {node_id}")
    fragment = layout.fragments[node_id]
    if fragment.state != "realized" or gates[node_id].state != "realized":
        raise OwnershipAuditError(f"artifact and gate lifecycle must both be realized for {node_id}")
    if fragment.generated:
        raise OwnershipAuditError("generated grants require the F02 ownership ledger audit")

    resolved_base = _resolve_commit(base)
    expected: dict[str, str] = {}
    for status, paths in (
        ("A", fragment.introduced),
        ("M", fragment.modified),
        ("D", tuple(item.path for item in fragment.removed)),
    ):
        for path in paths:
            if path in expected:
                raise OwnershipAuditError(f"artifact path appears in multiple categories: {path}")
            expected[path] = status
    actual = _changed_paths(resolved_base)
    lifecycle_paths = (
        {
            f"tests/fixtures/artifact_layout/nodes/{candidate}.json"
            for candidate in layout.node_ids
        }
        | {
            f"tests/fixtures/diagnostic_node_gates/{candidate}.json"
            for candidate in layout.node_ids
        }
        if node_id == "F00"
        else {
            f"tests/fixtures/artifact_layout/nodes/{node_id}.json",
            f"tests/fixtures/diagnostic_node_gates/{node_id}.json",
        }
    )
    lifecycle_status = "A" if node_id == "F00" else "M"
    wrong_lifecycle = sorted(
        path for path in lifecycle_paths if actual.get(path) != lifecycle_status
    )
    if wrong_lifecycle:
        raise OwnershipAuditError(
            f"node {node_id} lifecycle files are not exact: {wrong_lifecycle}"
        )
    for path in lifecycle_paths:
        del actual[path]
    if actual != expected:
        missing = sorted(set(expected) - set(actual))
        extra = sorted(set(actual) - set(expected))
        wrong = sorted(
            path for path in set(actual) & set(expected) if actual[path] != expected[path]
        )
        raise OwnershipAuditError(
            f"node {node_id} diff is not exact: missing={missing}, extra={extra}, wrong_status={wrong}"
        )

    for removed in fragment.removed:
        try:
            previous = subprocess.run(
                ["git", "show", f"{resolved_base}:{removed.path}"],
                cwd=ROOT,
                check=True,
                capture_output=True,
            ).stdout
        except subprocess.CalledProcessError as error:
            raise OwnershipAuditError(f"removed path did not exist at base: {removed.path}") from error
        if hashlib.sha256(previous).hexdigest() != removed.sha256:
            raise OwnershipAuditError(f"removed path preimage hash differs: {removed.path}")


def _parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description="Audit a diagnostics node against its artifact fragment")
    parser.add_argument("--node", required=True)
    parser.add_argument("--base", required=True)
    return parser


def main(argv: list[str] | None = None) -> int:
    arguments = _parser().parse_args(argv)
    try:
        audit_node(arguments.node, arguments.base)
    except OwnershipAuditError as error:
        print(f"diagnostic ownership audit: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
