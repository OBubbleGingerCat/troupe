# Pinned Perfetto compatibility tools

This manifest pins the official Perfetto `v57.2` release at commit
`da1d152cff27890903d158fe96751de3aab883cc`. The release page and GitHub
release-asset SHA-256 digests are authoritative:

`https://github.com/google/perfetto/releases/tag/v57.2`

The cache contains exactly one host tools archive and the shared
`perfetto-ui.zip` archive. Supported host platform IDs are `linux-amd64`,
`linux-arm`, `linux-arm64`, `mac-amd64`, `mac-arm64`, and
`windows-amd64`. The fetcher detects the host by default; `--platform` is
available for explicit CI selection.

Provision the dedicated external cache once:

```console
scripts/fetch_pinned_perfetto_tools.sh \
  --manifest tests/perfetto/tools/manifest.json \
  --cache "$TROUPE_PERFETTO_CACHE" \
  --provision
```

The cache path must be an existing canonical absolute directory outside the
repository, owned by the current user, and named for the selected platform.
Provisioning downloads to an exclusive temporary file in that directory,
checks SHA-256, publishes by same-directory atomic rename, and reuses a valid
member without transport access. Successful provisioning removes write bits from
the archives and identity, then freezes the cache directory at mode `0555`. A
complete writable staging cache can be frozen without another download; a frozen
cache is never made writable to repair missing or damaged content.

Every blocking compatibility job verifies the already-provisioned cache
strictly offline:

```console
scripts/fetch_pinned_perfetto_tools.sh \
  --offline --verify-only \
  --cache "$TROUPE_PERFETTO_CACHE"
```

Missing, writable, mismatched, symlinked, escaped, or unexpected cache members,
or a writable cache directory, fail closed. The archives remain only in this
external cache; they are not source files, sdist/wheel inputs, runtime
dependencies, or PATH fallbacks.

The current public UI probe is deliberately separate and non-blocking:

```console
scripts/fetch_pinned_perfetto_tools.sh --current-public-ui-canary
```

That scheduled canary always reports upstream/network failures as warnings.
Pinned offline cache verification is the blocking release result.
