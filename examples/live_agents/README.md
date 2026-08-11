# Live agent examples

These examples exercise provider CLIs and are intentionally excluded from the
ordinary deterministic example suite. The provider must already be logged in;
Troupe does not perform authentication.

For Codex, provide an explicit profile whose workspace is an existing writable
directory. The live harness creates and removes its own child workspace there.
`effort` is required but may be `null`.

```console
export TROUPE_LIVE_CODEX_PROFILE='{"workspace":"/tmp","model":"gpt-5.6-sol","effort":"max"}'
scripts/test_live_agent.sh codex
```

The harness uses the pinned Codex ACP adapter, verifies two contextual turns,
workspace tool use, built-in and custom schema correction, caller cancellation,
configuration failures, an isolated unlogged authentication failure, and
process cleanup. It does not print the profile or inherited credential
environment.

Claude uses the same explicit profile shape and must already be logged in with
Claude Code. Its live harness requires `bubblewrap` so it can replace the user
settings file inside the child mount namespace without changing the real file.

```console
export TROUPE_LIVE_CLAUDE_PROFILE='{"workspace":"/tmp","model":"sonnet","effort":"max"}'
scripts/test_live_agent.sh claude
```

The Claude case verifies its bundled Claude Code harness, eager HTTP MCP use,
two contextual turns, built-in and custom schema correction, automatic tool
permission handling, user/project/local settings and hooks, settings
precedence, typed configuration and authentication failures, conservative
cancellation settlement, and process cleanup. Neither live runner performs an
authentication flow.
