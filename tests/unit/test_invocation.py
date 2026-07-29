from __future__ import annotations

import importlib
import os

import pytest


def _parse(argv: list[str]) -> tuple[str, list[str]]:
    runtime = importlib.import_module("troupe._runtime")
    result = runtime._parse_invocation(argv)
    assert type(result) is tuple
    assert len(result) == 2
    path, production_args = result
    assert type(path) is str
    assert type(production_args) is list
    return path, production_args


@pytest.mark.parametrize(
    ("argv", "expected_path", "expected_args"),
    [
        (["--production", "p"], "p", []),
        (["--production", "\udcff"], "\udcff", []),
        (
            ["--production", "p", "--", "--x", "1", "in.txt"],
            "p",
            ["--x", "1", "in.txt"],
        ),
        (
            ["--production", "p", "--", "--x", "1", "--", "z"],
            "p",
            ["--x", "1", "--", "z"],
        ),
        (["--production", "p", "--", "\udcff"], "p", ["\udcff"]),
    ],
)
def test_parse_invocation_contract(
    argv: list[str], expected_path: str, expected_args: list[str]
) -> None:
    original = list(argv)

    path, production_args = _parse(argv)

    assert path == expected_path
    assert production_args == expected_args
    assert argv == original
    assert production_args is not argv


def test_first_exact_separator_splits_and_preserves_token_identity() -> None:
    separator = "".join(["-", "-"])
    second_separator = "".join(["-", "-"])
    value = "".join(["not", "-interned"])
    surrogate = "\udcff"
    argv = [
        "--production",
        "p",
        separator,
        value,
        second_separator,
        surrogate,
    ]
    original_ids = [id(token) for token in argv]

    _, production_args = _parse(argv)

    assert production_args == [value, second_separator, surrogate]
    assert all(actual is expected for actual, expected in zip(production_args, argv[3:]))
    assert [id(token) for token in argv] == original_ids


def test_surrogateescape_production_path_round_trips_through_os_bytes() -> None:
    path, production_args = _parse(["--production", "\udcff"])

    assert path == "\udcff"
    assert os.fsencode(path) == b"\xff"
    assert production_args == []


@pytest.mark.parametrize(
    "argv",
    [
        [],
        ["--production"],
        ["unexpected"],
        ["--production", "p", "extra"],
        ["--production", "p", "--x"],
    ],
)
def test_clap_usage_errors_are_value_errors(argv: list[str]) -> None:
    with pytest.raises(ValueError):
        _parse(argv)


@pytest.mark.parametrize("argv", [(), "--production", None])
def test_parse_invocation_requires_a_list(argv: object) -> None:
    runtime = importlib.import_module("troupe._runtime")

    with pytest.raises(TypeError):
        runtime._parse_invocation(argv)


def test_parse_invocation_validates_tokens_on_both_sides() -> None:
    runtime = importlib.import_module("troupe._runtime")

    for argv in (["--production", 1], ["--production", "p", "--", 1]):
        with pytest.raises(TypeError):
            runtime._parse_invocation(argv)
