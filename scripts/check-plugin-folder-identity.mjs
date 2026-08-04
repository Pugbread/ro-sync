import assert from "node:assert/strict";
import fs from "node:fs";

const source = fs.readFileSync(new URL("../plugin/Plugin.luau", import.meta.url), "utf8");
const assignmentPolicy = source.slice(
  source.indexOf("function snapshotApplyState.matchesCachedDiskIdentity"),
  source.indexOf("function snapshotApplyState.createInstance"),
);

assert.ok(assignmentPolicy.length > 0, "snapshot assignment policy must remain discoverable");
assert.match(
  assignmentPolicy,
  /candidateDiskPath\[#candidateDiskPath\] == node\.diskFragment/,
  "a claimed live instance may be reused only for its cached physical disk path",
);
assert.match(
  assignmentPolicy,
  /ctx\.claimedInstances\[candidate\][\s\S]*?matchesCachedDiskIdentity\(parent, candidate, node\)/,
  "repeated live operations for one exact disk identity must remain idempotent",
);
assert.match(
  assignmentPolicy,
  /node\.diskFragmentIsDir == true[\s\S]*?bucket\.byClass\.Folder[\s\S]*?not candidate and className == "Folder"[\s\S]*?bucket\.passThrough/,
  "an exact directory operation must reuse an unprojected empty container",
);
assert.match(
  assignmentPolicy,
  /not unprojectedOnly or not projected/,
  "the directory fallback must not steal a projected sibling with another fragment",
);

function sameCachedIdentity(parentPath, candidatePath, fragment) {
  return candidatePath.length === parentPath.length + 1
    && parentPath.every((segment, index) => candidatePath[index] === segment)
    && candidatePath.at(-1) === fragment;
}

const parentPath = ["ReplicatedStorage", "Client"];
const cachedFolder = [...parentPath, "DoubleJumps"];
assert.equal(
  sameCachedIdentity(parentPath, cachedFolder, "DoubleJumps"),
  true,
  "a repeated set for the same new directory must reuse its first empty parent",
);
assert.equal(
  sameCachedIdentity(parentPath, cachedFolder, "DoubleJumps [1]"),
  false,
  "a distinct duplicate-name physical fragment must not reuse the first parent",
);
assert.equal(
  sameCachedIdentity(["ReplicatedStorage", "Shared"], cachedFolder, "DoubleJumps"),
  false,
  "an identical fragment under a different parent must not reuse the cached folder",
);
