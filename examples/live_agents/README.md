# Live agent examples

These examples exercise provider CLIs and are intentionally excluded from the
ordinary deterministic example suite. The provider must already be logged in;
Troupe does not perform authentication.

Codex and Claude require Node.js with npm and `npx` because Troupe launches
their pinned ACP adapter packages through `npx`. Kimi requires the pinned Kimi
Code 0.31.1 CLI. All three CLIs must be installed and logged in before the
Production starts.

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

Kimi uses its built-in ACP server. The runner requires the adapter-pinned Kimi
Code 0.31.1 binary and an existing Kimi Code login; it accepts the same profile
shape.

```console
export TROUPE_LIVE_KIMI_PROFILE='{"workspace":"/tmp","model":"kimi-code/k3","effort":"max"}'
scripts/test_live_agent.sh kimi
```

The runner resolves exactly version 0.31.1, places it on an isolated child
`PATH`, and copies only the login material needed by a temporary
`KIMI_CODE_HOME`. It verifies the Read, Write, Bash, and AskUserQuestion
harness, unattended permission handling, contextual turns, both schema
correction paths, authoritative cancellation recovery, typed configuration and
authentication failures, wire evidence, and process cleanup. Troupe never
starts a Kimi authentication flow.

The mixed repository-repair example runs all three providers in one real
Production. Codex investigates a deterministic defect, Claude reviews the
behavior contract, Kimi repairs and commits the implementation, and the same
Codex Actor later recalls a random investigation identifier from its persistent
session. All three CLIs must already be logged in, and `bubblewrap` is required
to isolate Claude's user settings during the run.

```console
export TROUPE_LIVE_CODEX_PROFILE='{"workspace":"/tmp","model":"gpt-5.6-sol","effort":"max"}'
export TROUPE_LIVE_CLAUDE_PROFILE='{"workspace":"/tmp","model":"sonnet","effort":"max"}'
export TROUPE_LIVE_KIMI_PROFILE='{"workspace":"/tmp","model":"kimi-code/k3","effort":"max"}'
scripts/test_live_mixed_agents.sh
```

The external oracle creates and removes its own temporary Git repository under
the Codex profile workspace. It verifies typed results, Effect ownership,
context retention, the exact one-file commit, repository tests, clean shutdown,
and child-process cleanup. The three profile workspace fields are base
directories for this harness; the Production receives profiles rewritten to
the temporary repository itself.
