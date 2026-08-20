from __future__ import annotations

from decimal import Decimal

from troupe import diagnostics


def record_batch(*, queue_depth: int, region: str) -> None:
    with diagnostics.span(
        "example.process_batch",
        attributes={"region": region},
    ):
        diagnostics.counter(
            "example.queue_depth",
            queue_depth,
            unit="items",
            dimensions={"region": region},
        )
        diagnostics.event(
            "example.batch_ready",
            severity="info",
            attributes={
                "region": region,
                "ratios": (Decimal("0.5"), Decimal("0.75")),
            },
        )


def main() -> None:
    # record_batch() requires an active Runtime task, so direct execution only
    # validates the reusable example's ordinary Python values.
    assert Decimal("0.75").is_finite()
    assert callable(record_batch)


if __name__ == "__main__":
    main()
