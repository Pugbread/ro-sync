import assert from "node:assert/strict";
import { mergeProjectInitEvent } from "../project-init.js";

const created = mergeProjectInitEvent(
  { projects: [], servedProjectIds: [] },
  {
    requestId: "studio-123-456",
    projectPath: "/projects/Race Stars",
    name: "Race Stars",
    gameId: "123",
    placeId: "456",
    groupId: "9",
    creatorType: "Group",
    creatorId: "9",
  },
  () => "p_race",
);
assert.equal(created.created, true);
assert.equal(created.project.id, "p_race");
assert.deepEqual(created.patch.servedProjectIds, ["p_race"]);
assert.equal(created.patch.activeProjectId, "p_race");
assert.equal(created.project.settings.InitialSyncPriority, undefined);
assert.equal(created.project.creatorType, "Group");

const genericDataModelName = mergeProjectInitEvent(
  { projects: [], servedProjectIds: [] },
  {
    projectPath: "/projects/Race Stars",
    name: "Race Stars",
    gameName: "Place1",
    placeName: "Race Stars - Main",
    gameId: "123",
    placeId: "456",
  },
  () => "p_named",
);
assert.equal(genericDataModelName.project.name, "Race Stars");
assert.equal(genericDataModelName.project.gameName, "Place1");
assert.equal(genericDataModelName.project.placeName, "Race Stars - Main");

const customDisplayName = mergeProjectInitEvent(
  {
    projects: [{
      id: "p_custom",
      path: "/projects/Race Stars",
      name: "Race Stars 2",
      gameId: "123",
      placeIds: ["456"],
    }],
    servedProjectIds: [],
  },
  {
    projectPath: "/projects/Race Stars",
    name: "Race Stars",
    gameName: "Race Stars",
    placeName: "Main Place",
    gameId: "123",
    placeId: "456",
  },
);
assert.equal(customDisplayName.project.name, "Race Stars 2");
assert.equal(customDisplayName.project.gameName, "Race Stars");

const placeholderDisplayName = mergeProjectInitEvent(
  {
    projects: [{
      id: "p_placeholder",
      path: "/projects/Race Stars",
      name: "Place1",
      gameId: "123",
      placeIds: ["456"],
    }],
    servedProjectIds: [],
  },
  {
    projectPath: "/projects/Race Stars",
    name: "Race Stars",
    gameName: "Race Stars",
    placeName: "Main Place",
    gameId: "123",
    placeId: "456",
  },
);
assert.equal(placeholderDisplayName.project.name, "Race Stars");

const repeated = mergeProjectInitEvent(
  created.patch,
  {
    requestId: "retry",
    projectPath: "/projects/Race Stars",
    name: "Race Stars",
    gameId: "123",
    placeId: "789",
  },
  () => "must-not-be-used",
);
assert.equal(repeated.created, false);
assert.equal(repeated.patch.projects.length, 1);
assert.deepEqual(repeated.project.placeIds, ["456", "789"]);
assert.deepEqual(repeated.patch.servedProjectIds, ["p_race"]);

const daemonEvent = mergeProjectInitEvent(
  { projects: [], servedProjectIds: [] },
  {
    type: "project-init",
    project: "/projects/Daemon Created",
    directoryName: "daemon-created",
    metadata: {
      gameName: "Daemon Created",
      gameId: "800",
      placeId: "801",
      creatorType: "User",
      creatorId: "802",
    },
  },
  () => "p_daemon",
);
assert.equal(daemonEvent.project.path, "/projects/Daemon Created");
assert.equal(daemonEvent.project.name, "Daemon Created");
assert.equal(daemonEvent.project.creatorId, "802");

assert.throws(
  () => mergeProjectInitEvent({}, { projectPath: "relative", gameId: "0", placeId: "x" }),
  /missing its project path or Roblox IDs/,
);

console.log("project initialization policy checks passed");
