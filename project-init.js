// Pure state merge for project-initialization requests accepted by the native
// desktop broker. Keeping this deterministic makes renderer reloads and
// repeated Studio requests idempotent.

export function mergeProjectInitEvent(state, event, makeId = defaultProjectId) {
  const current = state && typeof state === "object" ? state : {};
  const projects = Array.isArray(current.projects) ? current.projects : [];
  const metadata = event?.metadata && typeof event.metadata === "object"
    ? event.metadata
    : {};
  const path = cleanString(event?.projectPath) || cleanString(event?.project);
  const gameId = cleanId(event?.gameId) || cleanId(metadata.gameId);
  const placeId = cleanId(event?.placeId) || cleanId(metadata.placeId);
  if (!path || !gameId || !placeId) {
    throw new Error("project initialization event is missing its project path or Roblox IDs");
  }

  const existingIndex = projects.findIndex((project) =>
    project?.path === path || (gameId && cleanId(project?.gameId) === gameId)
  );
  const existing = existingIndex >= 0 ? projects[existingIndex] : null;
  const id = cleanString(existing?.id) || makeId();
  const placeIds = new Set(
    Array.isArray(existing?.placeIds)
      ? existing.placeIds.map(cleanId).filter(Boolean)
      : [],
  );
  placeIds.add(placeId);
  const existingName = cleanString(existing?.name);
  const incomingName = cleanString(event?.name)
    || cleanString(metadata.gameName)
    || cleanString(metadata.name)
    || cleanString(event?.directoryName);
  const project = {
    ...(existing || {}),
    id,
    name: (existingName && !isPlaceholderProjectName(existingName) ? existingName : null)
      || incomingName
      || existingName
      || basename(path),
    path,
    addedAt: Number(existing?.addedAt) || Date.now(),
    gameId,
    gameName: cleanString(event?.gameName)
      || cleanString(metadata.gameName)
      || cleanString(existing?.gameName)
      || null,
    placeName: cleanString(event?.placeName)
      || cleanString(metadata.placeName)
      || cleanString(existing?.placeName)
      || null,
    groupId: cleanId(event?.groupId) || cleanId(metadata.groupId) || cleanId(existing?.groupId) || null,
    placeIds: [...placeIds],
    creatorType: cleanCreatorType(event?.creatorType)
      || cleanCreatorType(metadata.creatorType)
      || existing?.creatorType
      || null,
    creatorId: cleanId(event?.creatorId)
      || cleanId(metadata.creatorId)
      || cleanId(existing?.creatorId)
      || null,
    initializedRequestId: cleanString(event?.requestId) || existing?.initializedRequestId || null,
    settings: {
      AutoReconnect: "on",
      ...(existing?.settings || {}),
    },
    initializedFromStudio: true,
  };
  const nextProjects = [...projects];
  if (existingIndex >= 0) nextProjects[existingIndex] = project;
  else nextProjects.push(project);

  const served = new Set(
    Array.isArray(current.servedProjectIds)
      ? current.servedProjectIds.filter((value) => typeof value === "string")
      : [],
  );
  served.add(id);
  return {
    project,
    created: existingIndex < 0,
    patch: {
      projects: nextProjects,
      servedProjectIds: [...served],
      activeProjectId: id,
    },
  };
}

function cleanString(value) {
  return typeof value === "string" && value.trim() ? value.trim() : null;
}

function cleanId(value) {
  const text = cleanString(value);
  return text && /^\d{1,20}$/.test(text) && text !== "0" ? text : null;
}

function cleanCreatorType(value) {
  const text = cleanString(value);
  return text === "User" || text === "Group" ? text : null;
}

function isPlaceholderProjectName(value) {
  const normalized = String(value || "").trim().toLowerCase();
  return !normalized
    || normalized === "game"
    || normalized === "untitled experience"
    || /^place\d*$/.test(normalized);
}

function basename(path) {
  return String(path || "").replace(/[\\/]+$/, "").split(/[\\/]/).pop() || "project";
}

function defaultProjectId() {
  return "p_" + Date.now().toString(36) + Math.random().toString(36).slice(2, 8);
}
