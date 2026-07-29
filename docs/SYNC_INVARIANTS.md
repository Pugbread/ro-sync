# Sync correctness invariants

This document is the compatibility contract for Ro Sync's filesystem
projection, reconciliation engine, and Studio transport. A change that breaks
one of these invariants requires a protocol or projection-schema migration,
not a best-effort fallback.

## Projection

1. **The projection is bijective.** Every emitted filesystem node must decode
   to the same Roblox name, class, and parent shape. Reserved carrier syntax
   such as `init (...)` must be escaped when it is a literal leaf name.
2. **Logical and physical identity stay separate.** User-facing Studio paths
   identify instances logically. Mutating operations also carry the exact
   allocated disk fragment so duplicate names, encoded names, and
   script-with-children carriers cannot be reconstructed ambiguously.
3. **Carrier files are canonical.** A named init file identifies its parent
   only when its decoded inner name matches the decoded parent directory name
   after allocator disambiguation. A mismatched file is a literal leaf.
4. **Unsupported ambiguity fails closed.** If two nodes cannot be paired
   deterministically, reconciliation reports an actionable conflict. It must
   never silently choose a sibling based only on enumeration order.
5. **Service directories are containers.** Top-level DataModel services are
   not counted as projected instances. Empty service directories therefore
   represent an empty disk projection.

## Reconciliation

Every initial or recovery sync belongs to one monotonically increasing
generation:

```text
scan -> plan -> stage -> validate -> publish -> persist baseline -> acknowledge
```

- A generation is acknowledged only after all selected services and the
  conflict baseline are durable.
- A failed publish rolls back every service already published by that
  generation. Mixed generations are never reported as success.
- `skipped`, unresolved, or conflicted Source writes are failed operations,
  not successful no-ops.
- Script Source is written to its exact source carrier: the leaf file or the
  unique init file inside a script-with-children directory.
- A clean initial comparison returns a bounded daemon-authored receipt from
  dense Studio record IDs to exact disk fragments. The plugin installs that
  receipt before live hooks start; it never reconstructs duplicate identities
  from mutable Source or enumeration order.
- Reconnect recovery uses operation IDs and the last acknowledged generation.
  A transport gap must not silently discard unacknowledged edits.

## Filesystem events

- Echo suppression is scoped to exact daemon-authored paths. A service root,
  ancestor path, or project-wide elapsed-time window must never suppress an
  unrelated descendant edit.
- Create/update batches are normalized parent-first; delete batches are
  normalized child-first. Adding a directory also rescans its projected
  subtree so platform-specific notification order cannot lose children.
- Debouncing is paid once per coalesced batch, never once per destructive
  entry.

## Studio editor

- Remote Source application suppresses only the matching expected Source event
  for the same script.
- A pending local editor debounce is invalidated or rebased before a remote
  Source is applied, and the editor text is re-read immediately before commit.
- Failure to read authoritative editor text aborts/retries the coherent scan or
  push. It must never be converted to an empty script.

## Transport and lifecycle

- Every queue is bounded and returns an explicit overload result.
- Connect, authentication, request, socket write, and shutdown all have
  deadlines.
- Routed request expiry is derived from the caller's bounded request timeout
  plus a small completion grace. A fixed transport TTL must not undercut a
  valid workflow, playtest, or transmit deadline.
- Heartbeat supervision is independent from potentially blocked socket writes
  and grants a post-wake probe interval before declaring a peer dead.
- A transport timeout means **unresponsive**, not **stale**. Runtime ownership
  records are removed only after exact boot/process death is established.
- Project selection uses project/place identity. Lowest port is never an
  identity tiebreaker.

## Observability and release identity

Every session log and `/hello` response should identify the semantic version,
protocol, build commit, boot ID, connection ID, and project. Disconnects use a
typed reason and record queue depth, last-inbound age, and reconciliation
generation without logging capabilities or secrets.

Release verification must prove that the desktop host, daemon sidecar, Studio
plugin artifact, protocol, and generated documentation came from the same
commit. The embedded daemon and desktop versions must also equal the package,
installer, and release-tag version.

## Required regression matrix

- Literal `init`, `init (X)`, server/client variants, empty names, Unicode,
  case-only aliases, Windows device names, and allocator-looking `[N]` names.
- Leaf-to-directory and directory-to-leaf script transitions.
- Source edits in both directions for scripts with and without children.
- Same-name siblings with reversed Studio enumeration and distinct Sources.
- Editor debounce overlapping a remote apply and a second-script user edit.
- Empty existing service roots and first-time Studio-to-disk bootstrap.
- Child-before-parent filesystem notifications and large rename/delete bursts.
- Disconnect during every scan, stage, publish, baseline, and acknowledgement
  boundary.
- A non-reading WebSocket peer, queue overflow, laptop wake, and a delayed but
  still-live `/hello`.
- Two checkouts for one universe and multiple places within one universe.
