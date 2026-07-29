# troupe

Troupe runs a Python-defined production on a Rust/Tokio runtime. The supported
release target is Linux x86_64 glibc only.

## Install and run

Install Troupe into a Python environment:

```console
pip install troupe
```

After activating that environment, run a production package directly:

```console
troupe --production /path/to/my_production -- --value 7 input.txt
```

The direct command does not require `uv run`.
`uv run troupe` only selects the project environment when its executable
directory is not already on `PATH`.

Troupe ships one thin `troupe/__init__.py` wrapper. Loading, lifecycle control,
cancellation, signals, diagnostics, and the console command are
implemented in the Rust extension. The `troupe/__init__.pyi` stub describes
`Production`, the only public Python API.

## Production API

The constructor receives a `list[str]` equivalent to `sys.argv[1:]`: these are
the untouched tokens after `--` in the Troupe command. Construction is a
synchronous constructor so a Production can parse those tokens with
`argparse.ArgumentParser`. Lifecycle work belongs in async `start()`, `scene()`, and `stop()`.

```py
>>> import argparse
>>> import troupe
>>> class ExampleProduction(troupe.Production):
...     def __init__(self, args: list[str]) -> None:
...         parser = argparse.ArgumentParser(add_help=False)
...         parser.add_argument("--value", type=int, required=True)
...         self.options = parser.parse_args(args)
...
...     async def scene(self) -> None:
...         pass
>>> example = ExampleProduction(["--value", "7"])
>>> example.options.value
7
>>> type(example) is ExampleProduction
True

```

The runtime calls the hooks serially. A start failure means startup failed and
stop is not called. A non-cancellation scene failure is retained and stop still
runs. A scene `CancelledError` is normal shutdown; cancellation waits for the
scene's cleanup before stop. There is no cancellation grace period. A scene
that swallows `CancelledError` owns the resulting cleanup and completion delay.
In diagnostics, start, scene, and stop are separate failure phases.

The async ownership rules are:

- Scene is the only runtime-defined async work boundary.
- The runtime manages only the top-level scene task.
- Scene-owned work completes or is cancelled before scene returns.
- Cancellation is propagated and cleanup is awaited.
- Cross-scene work is managed by start and stop.
- Production chooses gather or another compatible task library.

Troupe does not define a subtask or task-group API.

## Production packages

A production path points to a Python package directory containing
`production.py` and its `Production` class. Its identity rules are:

- The directory basename is the real Python package name.
- Relative and absolute package imports keep the same identity.
- Package resources are available through importlib.resources.
- Module and pickle identity use `<package>.production.Production`.

The basename must be a valid, non-keyword Python identifier and must remain
unchanged by NFKC normalization. Other Python files, subpackages, and resources
inside that production directory are supported.

## Platform scope

The first release is Linux x86_64 glibc only, with a minimum
`manylinux_2_17_x86_64` (or equivalent `manylinux2014_x86_64`) wheel tag.
GIL-enabled CPython 3.10 through 3.14 is supported;
free-threaded CPython is not supported. macOS, Windows, musllinux, and other architectures are not supported.
