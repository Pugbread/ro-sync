# Playscript-owned race benchmark

This is the canonical `rosync playtest run` example: a client behaves like a
player by joining a queue and voting for a map, while a server script observes
AI laps, streams progress, and returns the final report. Replace the example
game-specific module and service paths with those from the place under test.

Run both scripts in one foreground session:

```sh
rosync playtest run --project . \
  --script ./docs/examples/playtest-run/bench.server.luau \
  --client-script ./docs/examples/playtest-run/join.client.luau \
  --mode multiplayer --players 1 \
  --args '{"map":"Lighthouse","laps":3}' \
  --timeout 600 --raw
```

The raw stream contains `ready`, `event`, and `clientResult` records followed by
one terminal `result`. On ordinary completion the playtest is already stopped
when the prompt returns. Pass `--keep-open` only when the runtime DataModel must
remain available for manual `playtest exec`, `logs`, or `capture` inspection.

## Runtime API used here

- `playtest.args` carries the decoded map/lap settings.
- `playtest.awaitClients` waits for a playable client context.
- `playtest.signal` and `playtest.await` coordinate the client vote with the
  server benchmark under the current job generation token.
- `playtest.emit` streams lap telemetry without an external poll loop.
- Returning from the server script supplies the final result and ends the run.

Every change made by these scripts lives only in Studio's disposable playtest
clone. Nothing syncs to disk or persists back into edit mode.
