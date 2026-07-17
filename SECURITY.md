# Security policy

## Reporting a vulnerability

Please do not open a public issue for a suspected vulnerability. Use the
repository's private [GitHub Security Advisory form](https://github.com/Pugbread/ro-sync/security/advisories/new)
and include the affected surface, reproduction, impact, and suggested fix when
known.

Never include a Roblox Open Cloud key, Ro Sync owner token, private game source,
or an unredacted runtime snapshot in a report.

## Trust boundaries

Ro Sync exposes a privileged loopback bridge between a local filesystem and
Roblox Studio. The daemon binds to loopback, browser clients require an owner
capability, Studio and CLI peers negotiate explicit protocol roles, and writes
are recorded in the Ro Sync audit log.

The Tauri renderer receives only narrowly scoped native commands. It must not be
given unrestricted shell or filesystem access. Desktop, Terminal 64, and CLI
surfaces must all verify the canonical project identity before reusing a daemon.

## Supported versions

Security fixes are applied to the latest release on `main`. Older development
builds may be asked to reproduce against the current release before a fix is
prepared.
