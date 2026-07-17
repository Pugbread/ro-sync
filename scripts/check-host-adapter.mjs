// Contract smoke test for the shared Terminal 64 / Tauri renderer adapter.
// This intentionally avoids a DOM dependency so it can run in CI with Node.

import assert from "node:assert/strict";

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

assert.deepEqual(await desktop.host.stateGet("state"), { activeProjectId: "p1" });
await desktop.host.daemonEnsure({
  project: "/tmp/ro-sync-project",
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
      preferredPort: 7878,
      gameId: "1",
      groupId: "2",
      placeIds: ["3"],
      ownerToken: "owner",
    },
  },
});

console.log("host adapter checks passed");
