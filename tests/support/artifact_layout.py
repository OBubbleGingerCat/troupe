from __future__ import annotations

import base64
import hashlib
import json
import re
from dataclasses import dataclass
from pathlib import Path, PurePosixPath
from typing import Any, Final

try:
    import tomllib
except ModuleNotFoundError:
    import tomli as tomllib


NODE_ID_RE: Final = re.compile(r"[A-Z][0-9]{2}")
SHA256_RE: Final = re.compile(r"[0-9a-f]{64}")
INDEX_FIELDS: Final = frozenset({"nodes"})
INDEX_NODE_FIELDS: Final = frozenset({"id", "artifact", "gate"})
BASE_FIELDS: Final = frozenset(
    {
        "rust_sources",
        "cargo_dependency_keys",
        "package_python_members",
        "package_stub_members",
        "package_files",
        "sdist_package_members",
        "wheel_members",
        "examples",
        "public_exports",
        "entry_points",
        "release_wheel_name",
    }
)
BYTE_SNAPSHOT_FIELDS: Final = frozenset({"base64", "sha256"})
ARTIFACT_FIELDS: Final = frozenset(
    {"state", "introduced", "modified", "removed", "generated"}
)
REMOVED_FIELDS: Final = frozenset({"path", "sha256"})
GATE_FIELDS: Final = frozenset(
    {
        "state",
        "argv",
        "env",
        "maturin_features",
        "cache_requirements",
        "exclusive_resources",
    }
)
ALLOWED_ROOTS: Final = frozenset(
    {"rust", "src", "tests", "scripts", "frontend", "docs", "examples"}
)
ALLOWED_ROOT_FILES: Final = frozenset(
    {".gitignore", "README.md", "pyproject.toml"}
)
ALLOWED_GATE_ENV: Final = frozenset(
    {
        "INTEGRATION_SHA",
        "PLAN_BUNDLE_SHA",
        "PRODUCT_BASE_SHA",
        "TROUPE_DIAGNOSTICS_EVIDENCE",
        "TROUPE_FINAL_ATTEMPT_ID",
        "TROUPE_GATE_TMP",
        "TROUPE_NPM_CACHE",
        "TROUPE_PERFETTO_CACHE",
        "TROUPE_PLAYWRIGHT_CACHE",
    }
)
ALLOWED_MATURIN_FEATURES: Final = frozenset(
    {"agent-test-support", "diagnostics-test-support"}
)
ALLOWED_CACHE_REQUIREMENTS: Final = frozenset({"npm", "perfetto", "playwright"})
ALLOWED_EXCLUSIVE_RESOURCES: Final = frozenset({"benchmark-host"})
GENERATED_GRANTS: Final = frozenset({"G01"})
POST_PLAN_EXAMPLE_CHANGES: Final = frozenset(
    {
        "README.md",
        "diagnostics/__init__.py",
        "diagnostics/production.py",
    }
)


def is_generated_example_path(path: PurePosixPath) -> bool:
    return any(part in {".troupe", "__pycache__"} for part in path.parts) or (
        path.suffix in {".pyc", ".pyo"}
    )


PLAN_INDEX_ROW_RE: Final = re.compile(
    r"\| (?P<node>[A-Z][0-9]{2}) \| .*? \| .*? \| [^|]+ \|$"
)


class ArtifactLayoutError(ValueError):
    pass


@dataclass(frozen=True, slots=True)
class ByteSnapshot:
    data: bytes
    sha256: str


@dataclass(frozen=True, slots=True)
class BaseArtifactContract:
    rust_sources: tuple[str, ...]
    cargo_dependency_keys: dict[str, tuple[str, ...]]
    package_python_members: tuple[str, ...]
    package_stub_members: tuple[str, ...]
    package_files: dict[str, ByteSnapshot]
    sdist_package_members: tuple[str, ...]
    wheel_members: tuple[str, ...]
    examples: dict[str, ByteSnapshot]
    public_exports: tuple[str, ...]
    entry_points: ByteSnapshot
    release_wheel_name: str


@dataclass(frozen=True, slots=True)
class RemovedArtifact:
    path: str
    sha256: str


@dataclass(frozen=True, slots=True)
class ArtifactFragment:
    state: str
    introduced: tuple[str, ...]
    modified: tuple[str, ...]
    removed: tuple[RemovedArtifact, ...]
    generated: tuple[str, ...]

    @property
    def static_paths(self) -> tuple[str, ...]:
        return self.introduced + self.modified + tuple(item.path for item in self.removed)


@dataclass(frozen=True, slots=True)
class GateDescriptor:
    state: str
    argv: tuple[tuple[str, ...], ...]
    env: dict[str, str]
    maturin_features: tuple[str, ...]
    cache_requirements: tuple[str, ...]
    exclusive_resources: tuple[str, ...]


@dataclass(frozen=True, slots=True)
class _IndexEntry:
    node_id: str
    artifact: str
    gate: str


@dataclass(frozen=True, slots=True)
class ArtifactLayout:
    node_ids: tuple[str, ...]
    base: BaseArtifactContract
    fragments: dict[str, ArtifactFragment]

    @property
    def paths(self) -> tuple[str, ...]:
        paths: list[str] = []
        for node_id in self.node_ids:
            fragment = self.fragments[node_id]
            if fragment.state == "realized":
                paths.extend(fragment.static_paths)
        return tuple(paths)

    def is_changed_after_base(self, path: str) -> bool:
        return any(
            fragment.state == "realized"
            and path in fragment.static_paths
            and node_id != "F00"
            for node_id, fragment in self.fragments.items()
        )


def _pairs_object(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
    result: dict[str, Any] = {}
    for key, value in pairs:
        if key in result:
            raise ArtifactLayoutError(f"duplicate JSON field: {key}")
        result[key] = value
    return result


def _read_json(path: Path) -> dict[str, Any]:
    try:
        raw = path.read_text(encoding="utf-8")
        value = json.loads(raw, object_pairs_hook=_pairs_object)
    except ArtifactLayoutError:
        raise
    except (OSError, UnicodeError, json.JSONDecodeError) as error:
        raise ArtifactLayoutError(f"could not read {path}: {error}") from error
    if not isinstance(value, dict):
        raise ArtifactLayoutError(f"{path} must contain a JSON object")
    return value


def _closed(value: dict[str, Any], expected: frozenset[str], context: str) -> None:
    actual = set(value)
    if actual != expected:
        missing = sorted(expected - actual)
        extra = sorted(actual - expected)
        raise ArtifactLayoutError(f"{context} fields are not closed: missing={missing}, extra={extra}")


def _string_list(
    value: Any,
    context: str,
    *,
    unique: bool = True,
) -> tuple[str, ...]:
    if not isinstance(value, list) or any(not isinstance(item, str) for item in value):
        raise ArtifactLayoutError(f"{context} must be a list of strings")
    result = tuple(value)
    if unique and len(result) != len(set(result)):
        raise ArtifactLayoutError(f"{context} contains a duplicate value")
    return result


def _mapping(value: Any, context: str) -> dict[str, Any]:
    if not isinstance(value, dict):
        raise ArtifactLayoutError(f"{context} must be an object")
    return value


def _canonical_repository_path(root: Path, value: str, context: str) -> str:
    if (
        not value
        or value.startswith("/")
        or "\\" in value
        or "\x00" in value
        or "\n" in value
        or any(part in {"", ".", ".."} for part in value.split("/"))
    ):
        raise ArtifactLayoutError(f"{context} is not a canonical repository path: {value!r}")
    path = PurePosixPath(value)
    if any(character in value for character in "*?[]{}"):
        raise ArtifactLayoutError(f"{context} must not contain a glob or subset rule: {value!r}")
    if value.endswith("/") or (root / value).is_dir():
        raise ArtifactLayoutError(f"{context} must name a file, not a directory: {value!r}")
    if len(path.parts) == 1:
        if value not in ALLOWED_ROOT_FILES:
            raise ArtifactLayoutError(f"{context} has an unknown repository root: {value!r}")
    elif path.parts[0] not in ALLOWED_ROOTS:
        raise ArtifactLayoutError(f"{context} has an unknown repository root: {value!r}")
    return value


def _byte_snapshot(value: Any, context: str) -> ByteSnapshot:
    mapping = _mapping(value, context)
    _closed(mapping, BYTE_SNAPSHOT_FIELDS, context)
    encoded = mapping["base64"]
    digest = mapping["sha256"]
    if not isinstance(encoded, str) or not isinstance(digest, str) or SHA256_RE.fullmatch(digest) is None:
        raise ArtifactLayoutError(f"{context} byte snapshot is malformed")
    try:
        data = base64.b64decode(encoded, validate=True)
    except ValueError as error:
        raise ArtifactLayoutError(f"{context} base64 is malformed") from error
    if hashlib.sha256(data).hexdigest() != digest:
        raise ArtifactLayoutError(f"{context} SHA-256 does not match its bytes")
    return ByteSnapshot(data=data, sha256=digest)


def _load_index(root: Path) -> tuple[_IndexEntry, ...]:
    index_path = root / "tests/fixtures/artifact_layout/index.json"
    value = _read_json(index_path)
    _closed(value, INDEX_FIELDS, str(index_path))
    raw_nodes = value["nodes"]
    if not isinstance(raw_nodes, list) or not raw_nodes:
        raise ArtifactLayoutError("artifact index nodes must be a non-empty list")
    entries: list[_IndexEntry] = []
    for position, raw_entry in enumerate(raw_nodes):
        entry = _mapping(raw_entry, f"artifact index node {position}")
        _closed(entry, INDEX_NODE_FIELDS, f"artifact index node {position}")
        node_id = entry["id"]
        artifact = entry["artifact"]
        gate = entry["gate"]
        if not all(isinstance(item, str) for item in (node_id, artifact, gate)):
            raise ArtifactLayoutError(f"artifact index node {position} values must be strings")
        if NODE_ID_RE.fullmatch(node_id) is None:
            raise ArtifactLayoutError(f"artifact index has an invalid node ID: {node_id!r}")
        expected_artifact = f"tests/fixtures/artifact_layout/nodes/{node_id}.json"
        expected_gate = f"tests/fixtures/diagnostic_node_gates/{node_id}.json"
        if artifact != expected_artifact or gate != expected_gate:
            raise ArtifactLayoutError(f"artifact index paths are not exact for {node_id}")
        entries.append(_IndexEntry(node_id, artifact, gate))
    ids = [entry.node_id for entry in entries]
    if len(ids) != len(set(ids)):
        raise ArtifactLayoutError("artifact index contains a duplicate node ID")
    return tuple(entries)


def _load_base(root: Path) -> BaseArtifactContract:
    path = root / "tests/fixtures/artifact_layout/base.json"
    value = _read_json(path)
    _closed(value, BASE_FIELDS, str(path))

    rust_sources = _string_list(value["rust_sources"], "base rust_sources")
    for source in rust_sources:
        _canonical_repository_path(root, f"rust/{source}", "base rust source")

    raw_cargo = _mapping(value["cargo_dependency_keys"], "base cargo_dependency_keys")
    cargo: dict[str, tuple[str, ...]] = {}
    for manifest, raw_keys in raw_cargo.items():
        _canonical_repository_path(root, manifest, "base Cargo manifest")
        cargo[manifest] = _string_list(raw_keys, f"base dependencies for {manifest}")

    package_files = {
        name: _byte_snapshot(snapshot, f"base package file {name}")
        for name, snapshot in _mapping(value["package_files"], "base package_files").items()
    }
    examples = {
        name: _byte_snapshot(snapshot, f"base example {name}")
        for name, snapshot in _mapping(value["examples"], "base examples").items()
    }
    entry_points = _byte_snapshot(value["entry_points"], "base entry_points")
    release_wheel_name = value["release_wheel_name"]
    if not isinstance(release_wheel_name, str) or not re.fullmatch(
        r"troupe-0\.1\.0-cp310-abi3-manylinux_2_17_x86_64\.whl",
        release_wheel_name,
    ):
        raise ArtifactLayoutError("base release_wheel_name is malformed")
    return BaseArtifactContract(
        rust_sources=rust_sources,
        cargo_dependency_keys=cargo,
        package_python_members=_string_list(value["package_python_members"], "base package_python_members"),
        package_stub_members=_string_list(value["package_stub_members"], "base package_stub_members"),
        package_files=package_files,
        sdist_package_members=_string_list(value["sdist_package_members"], "base sdist_package_members"),
        wheel_members=_string_list(value["wheel_members"], "base wheel_members"),
        examples=examples,
        public_exports=_string_list(value["public_exports"], "base public_exports"),
        entry_points=entry_points,
        release_wheel_name=release_wheel_name,
    )


def _load_fragment(root: Path, node_id: str, path: Path) -> ArtifactFragment:
    value = _read_json(path)
    _closed(value, ARTIFACT_FIELDS, f"artifact fragment {node_id}")
    state = value["state"]
    if state not in {"planned", "realized"}:
        raise ArtifactLayoutError(f"artifact fragment {node_id} has an invalid state")
    introduced = _string_list(value["introduced"], f"{node_id}.introduced")
    modified = _string_list(value["modified"], f"{node_id}.modified")
    generated = _string_list(value["generated"], f"{node_id}.generated")
    unknown_grants = set(generated) - GENERATED_GRANTS
    if unknown_grants:
        raise ArtifactLayoutError(f"artifact fragment {node_id} has unknown generated grants: {sorted(unknown_grants)}")

    raw_removed = value["removed"]
    if not isinstance(raw_removed, list):
        raise ArtifactLayoutError(f"{node_id}.removed must be a list")
    removed: list[RemovedArtifact] = []
    for position, raw_item in enumerate(raw_removed):
        item = _mapping(raw_item, f"{node_id}.removed[{position}]")
        _closed(item, REMOVED_FIELDS, f"{node_id}.removed[{position}]")
        removed_path, digest = item["path"], item["sha256"]
        if not isinstance(removed_path, str) or not isinstance(digest, str) or SHA256_RE.fullmatch(digest) is None:
            raise ArtifactLayoutError(f"{node_id}.removed[{position}] is malformed")
        removed.append(RemovedArtifact(removed_path, digest))
    removed_paths = tuple(item.path for item in removed)
    if len(removed_paths) != len(set(removed_paths)):
        raise ArtifactLayoutError(f"{node_id}.removed contains a duplicate path")

    for category, paths in (
        ("introduced", introduced),
        ("modified", modified),
        ("removed", removed_paths),
    ):
        for artifact_path in paths:
            _canonical_repository_path(root, artifact_path, f"{node_id}.{category}")
    categories = [set(introduced), set(modified), set(removed_paths)]
    if any(categories[left] & categories[right] for left in range(3) for right in range(left + 1, 3)):
        raise ArtifactLayoutError(f"artifact fragment {node_id} repeats a path across categories")

    nonempty = bool(introduced or modified or removed or generated)
    if state == "planned" and nonempty:
        raise ArtifactLayoutError(f"planned artifact fragment {node_id} must be empty")
    if state == "realized" and not nonempty:
        raise ArtifactLayoutError(f"realized artifact fragment {node_id} is not closed")
    return ArtifactFragment(state, introduced, modified, tuple(removed), generated)


def _exact_json_files(directory: Path, expected: set[Path], context: str) -> None:
    try:
        actual = set(directory.iterdir())
    except OSError as error:
        raise ArtifactLayoutError(f"could not inspect {context}: {error}") from error
    if actual != expected:
        missing = sorted(path.name for path in expected - actual)
        extra = sorted(path.name for path in actual - expected)
        raise ArtifactLayoutError(f"{context} files are not exact: missing={missing}, extra={extra}")
    invalid = sorted(path.name for path in expected if path.is_symlink() or not path.is_file())
    if invalid:
        raise ArtifactLayoutError(f"{context} entries are not regular files: {invalid}")


def load_artifact_layout(repository_root: Path) -> ArtifactLayout:
    root = repository_root.resolve()
    entries = _load_index(root)
    expected = {root / entry.artifact for entry in entries}
    _exact_json_files(root / "tests/fixtures/artifact_layout/nodes", expected, "artifact fragment")
    fragments = {
        entry.node_id: _load_fragment(root, entry.node_id, root / entry.artifact)
        for entry in entries
    }
    return ArtifactLayout(
        node_ids=tuple(entry.node_id for entry in entries),
        base=_load_base(root),
        fragments=fragments,
    )


def _load_gate(node_id: str, path: Path) -> GateDescriptor:
    value = _read_json(path)
    _closed(value, GATE_FIELDS, f"gate descriptor {node_id}")
    state = value["state"]
    if state not in {"planned", "realized"}:
        raise ArtifactLayoutError(f"gate descriptor {node_id} has an invalid state")

    raw_argv = value["argv"]
    if not isinstance(raw_argv, list):
        raise ArtifactLayoutError(f"gate descriptor {node_id} argv must be a list")
    commands: list[tuple[str, ...]] = []
    for position, raw_command in enumerate(raw_argv):
        command = _string_list(
            raw_command,
            f"gate descriptor {node_id} argv[{position}]",
            unique=False,
        )
        if not command or any(not argument or "\x00" in argument or "\n" in argument for argument in command):
            raise ArtifactLayoutError(f"gate descriptor {node_id} argv[{position}] is malformed")
        commands.append(command)

    raw_env = _mapping(value["env"], f"gate descriptor {node_id} env")
    if any(not isinstance(key, str) or not isinstance(item, str) for key, item in raw_env.items()):
        raise ArtifactLayoutError(f"gate descriptor {node_id} env must map strings to strings")
    unknown_env = set(raw_env) - ALLOWED_GATE_ENV
    if unknown_env:
        raise ArtifactLayoutError(f"gate descriptor {node_id} has unknown env: {sorted(unknown_env)}")

    features = _string_list(value["maturin_features"], f"gate descriptor {node_id} maturin_features")
    caches = _string_list(value["cache_requirements"], f"gate descriptor {node_id} cache_requirements")
    resources = _string_list(value["exclusive_resources"], f"gate descriptor {node_id} exclusive_resources")
    if set(features) - ALLOWED_MATURIN_FEATURES:
        raise ArtifactLayoutError(f"gate descriptor {node_id} has an unknown maturin feature")
    if set(caches) - ALLOWED_CACHE_REQUIREMENTS:
        raise ArtifactLayoutError(f"gate descriptor {node_id} has an unknown cache requirement")
    if set(resources) - ALLOWED_EXCLUSIVE_RESOURCES:
        raise ArtifactLayoutError(f"gate descriptor {node_id} has an unknown exclusive resource")
    if resources and (node_id != "V05" or resources != ("benchmark-host",)):
        raise ArtifactLayoutError(f"gate descriptor {node_id} is not authorized for an exclusive resource")

    nonempty = bool(commands or raw_env or features or caches or resources)
    if state == "planned" and nonempty:
        raise ArtifactLayoutError(f"planned gate descriptor {node_id} must be empty")
    if state == "realized" and not commands:
        raise ArtifactLayoutError(f"realized gate descriptor {node_id} is not closed")
    return GateDescriptor(state, tuple(commands), dict(raw_env), features, caches, resources)


def load_gate_descriptors(repository_root: Path) -> dict[str, GateDescriptor]:
    root = repository_root.resolve()
    entries = _load_index(root)
    expected = {root / entry.gate for entry in entries}
    _exact_json_files(root / "tests/fixtures/diagnostic_node_gates", expected, "gate descriptor")
    return {entry.node_id: _load_gate(entry.node_id, root / entry.gate) for entry in entries}


def validate_index_against_plan(repository_root: Path, plan_path: Path) -> None:
    root = repository_root.resolve()
    try:
        text = plan_path.read_text(encoding="utf-8")
        body = text.split("### 3.1 节点索引", 1)[1].split("### 3.2 依赖图", 1)[0]
    except (OSError, UnicodeError, IndexError) as error:
        raise ArtifactLayoutError("could not read the plan node index") from error
    plan_ids = tuple(
        match.group("node")
        for line in body.splitlines()
        if (match := PLAN_INDEX_ROW_RE.fullmatch(line)) is not None
    )
    index_ids = tuple(entry.node_id for entry in _load_index(root))
    if not plan_ids or len(plan_ids) != len(set(plan_ids)) or index_ids != plan_ids:
        raise ArtifactLayoutError("artifact index node IDs differ from the accepted plan")


def expected_rust_sources(layout: ArtifactLayout) -> tuple[str, ...]:
    paths = {f"rust/{path}" for path in layout.base.rust_sources}
    for fragment in layout.fragments.values():
        if fragment.state != "realized":
            continue
        paths.update(path for path in fragment.introduced if path.startswith("rust/") and path.endswith(".rs"))
        paths.difference_update(item.path for item in fragment.removed if item.path.endswith(".rs"))
    return tuple(sorted(path.removeprefix("rust/") for path in paths))


def expected_package_members(layout: ArtifactLayout, suffix: str) -> tuple[str, ...]:
    if suffix == ".py":
        members = set(layout.base.package_python_members)
    elif suffix == ".pyi":
        members = set(layout.base.package_stub_members)
    else:
        raise ArtifactLayoutError(f"unsupported package member suffix: {suffix}")
    prefix = "src/troupe/"
    for fragment in layout.fragments.values():
        if fragment.state != "realized":
            continue
        members.update(
            path.removeprefix(prefix)
            for path in fragment.introduced
            if path.startswith(prefix) and path.endswith(suffix)
        )
        members.difference_update(
            item.path.removeprefix(prefix)
            for item in fragment.removed
            if item.path.startswith(prefix) and item.path.endswith(suffix)
        )
    return tuple(sorted(members))


def validate_repository_artifacts(repository_root: Path, layout: ArtifactLayout) -> None:
    root = repository_root.resolve()
    actual_rust = tuple(
        sorted(
            path.relative_to(root / "rust").as_posix()
            for path in (root / "rust").rglob("*.rs")
            if "target" not in path.parts
        )
    )
    expected_rust = expected_rust_sources(layout)
    if actual_rust != expected_rust:
        raise ArtifactLayoutError("Rust source inventory differs from the artifact contract")

    for manifest, expected_keys in layout.base.cargo_dependency_keys.items():
        try:
            dependencies = tomllib.loads((root / manifest).read_text(encoding="utf-8"))["dependencies"]
        except (OSError, UnicodeError, KeyError, TypeError, tomllib.TOMLDecodeError) as error:
            raise ArtifactLayoutError(f"could not inspect dependencies in {manifest}") from error
        actual = set(dependencies)
        expected = set(expected_keys)
        if layout.is_changed_after_base(manifest):
            if not expected <= actual:
                raise ArtifactLayoutError(f"baseline Cargo dependency keys were removed from {manifest}")
        elif actual != expected:
            raise ArtifactLayoutError(f"Cargo dependency keys differ in {manifest}")

    for name, snapshot in layout.base.package_files.items():
        repository_path = f"src/troupe/{name}"
        if layout.is_changed_after_base(repository_path):
            continue
        try:
            actual = (root / repository_path).read_bytes()
        except OSError as error:
            raise ArtifactLayoutError(f"could not read baseline package file {name}") from error
        if actual != snapshot.data:
            raise ArtifactLayoutError(f"baseline package file was rewritten: {name}")

    actual_examples = {}
    for path in (root / "examples").rglob("*"):
        if not path.is_file():
            continue
        relative = PurePosixPath(path.relative_to(root / "examples").as_posix())
        if is_generated_example_path(relative):
            continue
        actual_examples[relative.as_posix()] = path.read_bytes()
    expected_example_names = set(layout.base.examples)
    for node_id in layout.node_ids:
        fragment = layout.fragments[node_id]
        if fragment.state != "realized" or node_id == "F00":
            continue
        for path in fragment.introduced:
            if path.startswith("examples/"):
                expected_example_names.add(path.removeprefix("examples/"))
        for removed in fragment.removed:
            if removed.path.startswith("examples/"):
                expected_example_names.discard(removed.path.removeprefix("examples/"))
    expected_example_names.update(POST_PLAN_EXAMPLE_CHANGES)
    if set(actual_examples) != expected_example_names:
        raise ArtifactLayoutError("example inventory differs from the artifact contract")
    for name, snapshot in layout.base.examples.items():
        if (
            name in expected_example_names
            and name not in POST_PLAN_EXAMPLE_CHANGES
            and not layout.is_changed_after_base(f"examples/{name}")
            and actual_examples[name] != snapshot.data
        ):
            raise ArtifactLayoutError(f"baseline example was rewritten: {name}")
