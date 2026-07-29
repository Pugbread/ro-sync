# Ro Sync architecture

Ro Sync is one local engine with three interchangeable control surfaces:

```text
Ro Sync Desktop ──┐
Terminal 64 ──────┼── localhost HTTP/WebSocket ── rosync daemon(s) ── Studio plugin(s)
rosync CLI ───────┘                                      │
                                                        └── project filesystem(s)
```

The desktop app and Terminal 64 widget use the same HTML, CSS, JavaScript views,
daemon client, and command documentation. They differ only in the small native
host adapter that provides persistence, dialogs, plugin installation, and
process lifecycle operations. CLI-only installations do not load the frontend.

## Components

The behavioral contract for projection, reconciliation, editor events,
transport, and lifecycle handling is defined in
[`SYNC_INVARIANTS.md`](./SYNC_INVARIANTS.md). Implementations and compatibility
changes must preserve those invariants across both Rust and Luau.

### Rust engine

The `rosync` executable contains both the foreground server (`rosync serve`) and
the complete CLI. The daemon owns filesystem watching, conflict tracking,
generated project context, artifact storage, workflow execution, and request
routing to Studio.

Managed installations use `rosync daemon start|status|stop|restart|logs`.
Runtime records live in the platform Ro Sync data directory rather than a UI's
private state. A record identifies the canonical project, port, PID, boot ID,
manager kind, log path, and authenticated control capability.

Project paths—not Game IDs—are the daemon identity. A command refuses an
occupied port that belongs to another canonical project. Stopping a daemon
requires a matching boot identity and authenticated graceful-shutdown request;
a stale PID record is never sufficient authority to kill a process.

### Shared frontend

`index.html`, `style.css`, `app.js`, and `views/` form a static ES-module
application. The frontend talks directly to the daemon's authenticated loopback
HTTP and WebSocket API for status, events, conflicts, and initial-sync choices.

Privileged operating-system behavior goes through a typed host interface:

- application and resource information
- state and secret storage
- project-scoped file reads and writes
- folder selection and opening
- clipboard writes
- plugin installation
- Wally installation
- daemon ensure, status, and stop

The interface deliberately contains no arbitrary shell command. Terminal 64
implements the operations with its host RPC. Tauri implements them as narrowly
scoped Rust commands.

### Tauri desktop shell

The desktop shell packages the shared frontend, the platform `rosync` sidecar,
the Studio plugin, generated command docs, and optional Luau tools. Native state
uses Application Support on macOS and AppData on Windows. Secrets use the
operating system's credential store where supported.

The Tauri webview receives only the main-window capability and registered Ro
Sync commands. It cannot spawn arbitrary processes or read arbitrary files.

Desktop tracks desired serving separately from UI focus and holds one exact
native ownership claim per canonical project. Starts are serialized only while
choosing a free listener; the resulting daemons run concurrently. A separate
loopback broker on ports 7867–7870 exists before any project daemon, allowing a
published Studio place to request safe creation beneath the user-authorized
Projects folder. The renderer adopts the queued project and starts its daemon;
the plugin never spawns a process itself.

### Roblox Studio plugin

The Studio plugin is identical for all three distributions. It discovers a
matching loopback daemon, validates the protocol and game binding, mirrors the
supported filesystem-backed classes, and serves structured inspection,
mutation, capture, and playtest requests.

Playtest runtime agents remain inside temporary PlayServer and PlayClient
DataModels. They communicate back through the edit-mode plugin and never become
independent localhost clients.

## Transport and trust

- The daemon binds only to loopback.
- Browser-backed surfaces require an unguessable owner capability in addition
  to an allowlisted local application origin.
- WebSocket peers declare one protocol role before receiving traffic.
- Binary artifacts use bounded, tokenized leases with size and hash checks.
- Studio mutations are written to the audit log.
- Desktop native commands validate every path and argument at the Rust boundary.

## Compatibility

`rosync serve` remains available for launchd, systemd, Task Scheduler,
containers, and debugging. Existing Terminal 64 lifecycle flags and routes are
kept as compatibility aliases while the shared manager terminology becomes the
canonical interface.

Project-root `ro-sync.json` remains portable between every distribution. Legacy
Terminal 64 state and credential locations are migration inputs, not permanent
runtime dependencies for Desktop or CLI-only installations.
