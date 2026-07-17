# `playtest run` Studio fixtures

These scripts exercise the user-visible playscript lifecycle against a connected
Roblox Studio fixture place. They are intentionally independent of the synced
DataModel and may be passed directly to `--script` / `--client-script`.

| Fixture | Expected contract |
| --- | --- |
| `ok.server.luau` | Emits once, returns a table, exits 0, and auto-stops. |
| `done-spawn.server.luau` | `playtest.done` from a spawned task wins while the main task waits. |
| `fail-spawn.server.luau` | `playtest.fail` from a spawned task exits 2. |
| `throw.server.luau` | Nested uncaught error reports a Luau traceback and exits 2. |
| `timeout-events.server.luau` | Emits partial progress until the CLI deadline stops it with exit 3. |
| `quiet.server.luau` | Emits nothing for 60 seconds; runtime heartbeats keep it attached. |
| `flood.server.luau` | Attempts 10,000 events; delivered events plus dropped counts equal 10,000. |
| `client-ok.client.luau` | Signals readiness and produces a successful `clientResult`. |
| `client-error.client.luau` | Produces a failed client event without ending the main run. |
| `client-side-data.server.luau` | Keeps the main run alive long enough to prove companion outcomes remain side data. |
| `fanout-server.server.luau` / `fanout-client.client.luau` | Waits for two clients and verifies both companion returns before teardown. |
| `signal-server.server.luau` / `signal-client.client.luau` | Round-trip a generation-scoped signal. |
| `oversized-result.server.luau` | Produces the explicit 1 MiB result truncation marker. |
| `logs.server.luau` | Produces info and warning output for `--logs` threshold checks. |

Examples:

```sh
rosync playtest run --project . \
  --script ./daemon/tests/fixtures/playtest-run/ok.server.luau --raw

rosync playtest run --project . \
  --script ./daemon/tests/fixtures/playtest-run/signal-server.server.luau \
  --client-script ./daemon/tests/fixtures/playtest-run/signal-client.client.luau \
  --mode multiplayer --players 1 --raw

rosync playtest run --project . \
  --script ./daemon/tests/fixtures/playtest-run/timeout-events.server.luau \
  --timeout 10 --raw
```

For external-stop coverage, start `quiet.server.luau`, press **Stop** in Studio,
and verify an `aborted` NDJSON record includes `jobStatus` before exit 4.
