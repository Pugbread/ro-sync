import assert from "node:assert/strict";

// projects.js imports the shared browser bridge. Its module setup only needs
// these inert browser globals; no renderer, native command, app, or daemon is
// started by this policy check.
globalThis.window = {
  addEventListener() {},
  parent: { postMessage() {} },
};

const {
  assessProjectPath,
  validateWallyFolder,
  wallyTomlPathForFolder,
} = await import("../views/projects.js");

const unauthorizedManual = assessProjectPath({
  isDesktop: true,
  source: "manual",
  path: "/tmp/project",
});
assert.equal(unauthorizedManual.ok, false);
assert.match(unauthorizedManual.message, /Browse/u);

assert.deepEqual(
  assessProjectPath({
    isDesktop: true,
    source: "manual",
    path: "/tmp/project/",
    authorizedPath: "/tmp/project",
  }),
  { ok: true, path: "/tmp/project" },
);

const desktopDrop = assessProjectPath({
  isDesktop: true,
  source: "drop",
  path: "/tmp/project",
  authorizedPath: "/tmp/project",
});
assert.equal(desktopDrop.ok, false);
assert.match(desktopDrop.message, /Browse/u);

assert.deepEqual(
  assessProjectPath({
    isDesktop: false,
    source: "drop",
    path: "/tmp/project/",
  }),
  { ok: true, path: "/tmp/project" },
);

assert.equal(
  validateWallyFolder("ReplicatedStorage/Assets/Packages"),
  "ReplicatedStorage/Assets/Packages",
);
assert.equal(
  validateWallyFolder("ReplicatedStorage\\Assets\\Packages"),
  "ReplicatedStorage/Assets/Packages",
);
assert.equal(
  wallyTomlPathForFolder("/game", "ReplicatedStorage/Assets/Packages"),
  "/game/ReplicatedStorage/Assets/wally.toml",
);
assert.equal(
  wallyTomlPathForFolder("C:\\game", "ReplicatedStorage/Assets/Packages"),
  "C:\\game\\ReplicatedStorage\\Assets\\wally.toml",
);

const rejectedWallyFolders = [
  "",
  ".",
  "..",
  "A/../B",
  "A/./B",
  "A//B",
  "A/ /B",
  "A/B/",
  "/A/B",
  "C:\\A",
  "A/\n/B",
  "A/\u007f/B",
];

for (const folder of rejectedWallyFolders) {
  assert.throws(
    () => validateWallyFolder(folder),
    undefined,
    `validator accepted unsafe Wally folder ${JSON.stringify(folder)}`,
  );
  assert.throws(
    () => wallyTomlPathForFolder("/game", folder),
    undefined,
    `path construction accepted unsafe Wally folder ${JSON.stringify(folder)}`,
  );
}

console.log("project path authorization and Wally path policy checks passed");
