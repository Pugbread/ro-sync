// Contract smoke test for the shared Terminal 64 / Tauri renderer adapter.
// This intentionally avoids a DOM dependency so it can run in CI with Node.

import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";

globalThis.window = {
  parent: { postMessage() {} },
  addEventListener() {},
};
Object.defineProperty(globalThis, "navigator", {
  configurable: true,
  value: { userAgent: "Macintosh" },
});

delete globalThis.__TAURI__;
const widget = await import("../bridge.js?host=terminal64-test");
assert.equal(widget.HOST_KIND, "terminal64");
assert.equal(widget.host.isDesktop, false);
assert.equal(widget.host.supports.spawnSession, true);
assert.equal(Object.hasOwn(widget.host, "exec"), false, "public adapter must not expose raw exec");

const calls = [];
globalThis.__TAURI__ = {
  core: {
    async invoke(command, args) {
      calls.push({ command, args });
      if (command === "state_get") return { value: { activeProjectId: "p1" } };
      if (command === "app_info") return { platform: "darwin", version: "test" };
      return { ok: true, running: true, port: 7878 };
    },
  },
};

const desktop = await import("../bridge.js?host=tauri-test");
assert.equal(desktop.HOST_KIND, "tauri");
assert.equal(desktop.host.isDesktop, true);
assert.equal(desktop.host.supports.spawnSession, false);
assert.equal(Object.hasOwn(desktop.host, "exec"), false, "desktop adapter must not expose raw exec");

desktop.setDaemonAuthToken("token-a", "http://127.0.0.1:7878");
desktop.setDaemonAuthToken("token-b", "http://127.0.0.1:7879");
assert.equal(new URL(desktop.daemonURL("http://127.0.0.1:7878", "/hello")).searchParams.get("widgetToken"), "token-a");
assert.equal(new URL(desktop.daemonURL("http://127.0.0.1:7879", "/hello")).searchParams.get("widgetToken"), "token-b");
desktop.setDaemonAuthToken(null, "http://127.0.0.1:7878");
assert.equal(new URL(desktop.daemonURL("http://127.0.0.1:7878", "/hello")).searchParams.get("widgetToken"), null);

assert.deepEqual(await desktop.host.stateGet("state"), { activeProjectId: "p1" });
await desktop.host.daemonEnsure({
  project: "/tmp/ro-sync-project",
  projectsRoot: "/tmp",
  preferredPort: 7878,
  gameId: "1",
  groupId: "2",
  placeIds: ["3"],
  ownerToken: "owner",
});

assert.deepEqual(calls.at(-1), {
  command: "daemon_ensure",
  args: {
    spec: {
      project: "/tmp/ro-sync-project",
      projectsRoot: "/tmp",
      preferredPort: 7878,
      gameId: "1",
      groupId: "2",
      placeIds: ["3"],
      ownerToken: "owner",
    },
  },
});

await desktop.host.daemonList();
assert.deepEqual(calls.at(-1), { command: "daemon_list", args: {} });

await desktop.host.daemonStop({
  project: "/tmp/ro-sync-project",
  bootId: "boot-id",
  ownerToken: "owner-token",
});
assert.deepEqual(calls.at(-1), {
  command: "daemon_stop",
  args: {
    spec: {
      project: "/tmp/ro-sync-project",
      bootId: "boot-id",
      ownerToken: "owner-token",
    },
  },
});

await desktop.host.projectBrokerStatus();
assert.deepEqual(calls.at(-1), { command: "project_broker_status", args: {} });

await desktop.host.projectInitDrain();
assert.deepEqual(calls.at(-1), { command: "project_init_drain", args: {} });

const nativeCommands = await readFile(new URL("../desktop/src-tauri/src/commands.rs", import.meta.url), "utf8");
assert.doesNotMatch(
  nativeCommands,
  /blocking_pick_folder/,
  "the native folder picker must not block AppKit's main thread",
);

const settingsSource = await readFile(new URL("../views/settings.js", import.meta.url), "utf8");
assert.doesNotMatch(
  settingsSource,
  /\.secretGet\s*\(/,
  "opening Settings must not eagerly read a Keychain credential",
);

console.log("host adapter checks passed");
