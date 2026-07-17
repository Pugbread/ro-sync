import assert from "node:assert/strict";
import fs from "node:fs";
import {
  canStopDesktopDaemon,
  isDesktopManagedStatus,
} from "../lifecycle-policy.js";

const exactStatus = {
  running: true,
  managed: true,
  externallyManaged: false,
  managedBy: "desktop",
  project: "/game",
  canonicalProject: "/game",
  pid: 4100,
  port: 7878,
  bootId: "desktop-boot",
};
const exactHello = {
  managed: true,
  managedBy: "desktop",
  project: "/game",
  pid: 4100,
  port: 7878,
  bootId: "desktop-boot",
};
const policy = (status, hello, ownershipAuthenticated = true) => canStopDesktopDaemon({
  status,
  hello,
  ownershipAuthenticated,
  expectedProjects: ["/game"],
});

assert.equal(isDesktopManagedStatus(exactStatus), true);
assert.equal(policy(exactStatus, exactHello), true, "exact authenticated Desktop boot may stop");
assert.equal(policy(exactStatus, exactHello, false), false, "unauthenticated Desktop boot must survive");
assert.equal(policy(exactStatus, { ...exactHello, bootId: "replacement" }), false, "replacement boot must survive");

for (const external of [
  { ...exactStatus, managedBy: "cli" },
  { ...exactStatus, managedBy: "other-manager" },
  { ...exactStatus, externallyManaged: true },
  { ...exactStatus, managed: false, managedBy: "manual" },
]) {
  assert.equal(isDesktopManagedStatus(external), false);
  assert.equal(policy(external, exactHello), false, "external daemon must never be stoppable");
}

const appSource = fs.readFileSync(new URL("../app.js", import.meta.url), "utf8");
const bridgeSource = fs.readFileSync(new URL("../bridge.js", import.meta.url), "utf8");
const prepareSource = fs.readFileSync(new URL("../desktop/scripts/prepare.mjs", import.meta.url), "utf8");
const tauriLibSource = fs.readFileSync(new URL("../desktop/src-tauri/src/lib.rs", import.meta.url), "utf8");
assert.equal(
  appSource.includes("host.daemonStop("),
  false,
  "renderer must not bypass authenticated HTTP ownership with native record-based stop",
);
assert.equal(
  appSource.includes('"--owner-token", ensureOwnerToken()'),
  false,
  "Terminal 64 launch must not put the owner token in argv",
);
assert.equal(
  bridgeSource.includes("async daemonStop("),
  false,
  "shared renderer host must not expose the unauthenticated native stop command",
);
assert.equal(
  tauriLibSource.includes("daemon::daemon_stop"),
  false,
  "Tauri invoke surface must not expose record-based native stop",
);
assert.equal(
  prepareSource.includes('"lifecycle-policy.js"'),
  true,
  "Desktop packaging must include the shared lifecycle policy module",
);

console.log("lifecycle policy checks passed");
