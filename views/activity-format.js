// Shared, privacy-conscious activity model and renderer for Projects + Activity.
// The daemon already emits sanitized envelopes; this module deliberately applies
// a second allowlist before retaining or displaying anything in the app.

const DETAIL_KEYS = new Set([
  "path", "from", "to", "property", "expectedClass", "propertyCount",
  "depth", "selector", "limit", "under", "class", "name", "attribute",
  "tag", "method", "argumentCount", "selectionCount", "transaction",
  "itemCount", "byteLength", "selectionMode", "parent",
  "action", "enum", "sourceBytes", "focus", "uiTarget", "ui", "view",
  "format", "mode", "players", "context", "identity", "operation",
  "jobId", "level",
]);
const PROJECT_KEYS = new Set([
  "status", "project", "projectPath", "directoryName", "name", "gameName",
  "placeName", "gameId", "groupId", "placeId", "creatorType", "creatorId",
  "wallyEnabled", "wallyFolder", "changed",
]);
const CONFIG_KEYS = new Set([
  "name", "gameName", "placeName", "gameId", "groupId", "placeId",
  "placeIds", "creatorType", "creatorId", "wallyEnabled", "wallyFolder",
]);
const MAX_TECHNICAL_STRING = 512;
let nextSequence = 1;

export function pushActivity(entries, frame, at = Date.now(), limit = 100) {
  const safeFrame = scrubActivityFrame(frame);
  if (!safeFrame) return null;

  const activityId = safeFrame.type === "command-activity" ? safeFrame.activityId : "";
  if (activityId) {
    const key = `command:${activityId}`;
    const existing = entries.find((entry) => entry.key === key);
    if (existing) {
      if (safeFrame.phase === "started") {
        // Defensive reset for legacy daemons whose process-local request IDs
        // could repeat after a restart. Current daemons include a boot scope.
        existing.at = at;
        existing.updatedAt = at;
        existing.frame = safeFrame;
        return existing;
      }
      existing.updatedAt = at;
      existing.frame = {
        ...existing.frame,
        ...safeFrame,
        detail: { ...(existing.frame.detail || {}), ...(safeFrame.detail || {}) },
      };
      return existing;
    }
    const entry = { key, at, updatedAt: at, frame: safeFrame };
    entries.push(entry);
    trimEntries(entries, limit);
    return entry;
  }

  const entry = {
    key: `${safeFrame.type}:${at}:${nextSequence++}`,
    at,
    updatedAt: at,
    frame: safeFrame,
  };
  entries.push(entry);
  trimEntries(entries, limit);
  return entry;
}

export function isCountableActivity(frame) {
  const type = frame && frame.type;
  return type === "sync-activity"
    || (type === "command-activity" && frame.phase === "started");
}

export function scrubActivityFrame(frame) {
  if (!frame || typeof frame !== "object" || Array.isArray(frame)) return null;
  const rawType = safeText(frame.type, 64) || "event";
  const type = /^[a-z0-9-]+$/.test(rawType) ? rawType : "event";

  if (type === "command-activity") {
    const rawActivityId = safeText(frame.activityId, 128);
    const activityId = /^request:(?:[a-f0-9]{16}:)?\d+$/.test(rawActivityId) ? rawActivityId : "";
    const phase = ["started", "completed", "aborted"].includes(frame.phase)
      ? frame.phase
      : "completed";
    const safe = {
      type,
      activityId: activityId || "",
      phase,
      op: safeOperation(frame.op),
      detail: pickPrimitiveFields(frame.detail, DETAIL_KEYS),
    };
    if (typeof frame.ok === "boolean") safe.ok = frame.ok;
    if (Number.isFinite(frame.durationMs) && frame.durationMs >= 0) {
      safe.durationMs = Math.min(Math.round(frame.durationMs), 86_400_000);
    }
    if (safe.ok === false || phase === "aborted") {
      safe.error = frame.error === "Requester disconnected" ? frame.error : "Command failed";
    }
    return safe;
  }

  if (type === "sync-activity") return scrubSyncActivity(frame);

  // Older daemons sent source-bearing `op` frames to watchers. Keep only the
  // small filesystem identity fields and normalize them into the new model.
  if (type === "op" && frame.op && typeof frame.op === "object") {
    const op = frame.op;
    return scrubSyncActivity({
      type: "sync-activity",
      op: op.op,
      path: op.path,
      from: op.from,
      to: op.to,
      class: op.class || op.node?.class,
      name: op.name || op.node?.name,
    });
  }

  if (type === "plugin") {
    return { type, connected: frame.connected === true };
  }
  if (type === "project-init") {
    const safe = pickPrimitiveFields(frame, PROJECT_KEYS);
    const metadata = pickPrimitiveFields(frame.metadata, PROJECT_KEYS);
    return { type, ...safe, ...(Object.keys(metadata).length ? { metadata } : {}) };
  }
  if (type === "config-changed") {
    return { type, ...pickPrimitiveFields(frame, CONFIG_KEYS) };
  }
  if (type === "initial-choice-needed") {
    return { type, choiceId: safeText(frame.choiceId, 128) || "" };
  }
  if (type === "initial-choice-made") {
    return { type, choice: safeText(frame.choice, 32) || "selected source" };
  }
  if (type === "conflict" || type === "sync-error") {
    return { type, path: safePath(frame.path) };
  }
  if (type === "shutdown") {
    return { type, reason: safeText(frame.reason, 120) || "Daemon stopped" };
  }
  if (type === "busy") {
    const count = Number.isFinite(frame.count) ? Math.max(1, Math.round(frame.count)) : 0;
    return { type, count };
  }
  if (type === "stream-error" || type === "error") {
    const attempts = Number.isFinite(frame.attempts) ? Math.max(1, Math.round(frame.attempts)) : 1;
    return { type: "stream-error", attempts };
  }
  return { type };
}

export function formatActivity(entry) {
  if (!entry || !entry.frame) return null;
  const frame = entry.frame;
  if (frame.type === "command-activity") return formatCommand(entry);
  if (frame.type === "sync-activity") return formatSync(entry);

  const base = baseModel(entry, "system", "info", "Done");
  switch (frame.type) {
    case "plugin":
      return {
        ...base,
        category: "connection",
        tone: frame.connected ? "success" : "warning",
        state: frame.connected ? "done" : "info",
        stateLabel: frame.connected ? "Connected" : "Offline",
        title: frame.connected ? "Studio connected" : "Studio disconnected",
        intent: frame.connected
          ? "The plugin is ready to receive Ro Sync actions."
          : "Ro Sync is waiting for the Studio plugin to reconnect.",
      };
    case "project-init": {
      const data = { ...(frame.metadata || {}), ...frame };
      const name = data.name || data.gameName || data.placeName || data.directoryName || "Studio project";
      const created = String(data.status || "").toLowerCase() === "created";
      return {
        ...base,
        category: "project",
        tone: "success",
        title: `${created ? "Created" : "Prepared"} ${name}`,
        intent: "Built the local project from the experience currently open in Studio.",
        target: safePath(data.project || data.projectPath),
        facts: compactFacts([
          fact("Game ID", data.gameId), fact("Place ID", data.placeId),
          fact("Group ID", data.groupId), fact("Folder", data.directoryName),
        ]),
      };
    }
    case "config-changed":
      return {
        ...base,
        category: "project",
        title: frame.name ? `Updated ${frame.name} settings` : "Updated project settings",
        intent: "Reloaded the project identity and sync configuration.",
        facts: compactFacts([
          fact("Game ID", frame.gameId), fact("Group ID", frame.groupId),
          fact("Places", Array.isArray(frame.placeIds) ? frame.placeIds.length : null),
          fact("Packages", typeof frame.wallyEnabled === "boolean" ? (frame.wallyEnabled ? "On" : "Off") : null),
        ]),
      };
    case "conflict":
      return {
        ...base,
        category: "warning",
        tone: "danger",
        state: "failed",
        stateLabel: "Needs review",
        title: `Sync conflict${frame.path ? ` in ${leaf(frame.path)}` : " detected"}`,
        intent: "The local file and Studio changed independently; choose which version to keep.",
        target: frame.path,
      };
    case "sync-error":
      return {
        ...base,
        category: "warning",
        tone: "danger",
        state: "failed",
        stateLabel: "Failed",
        title: `Couldn’t sync${frame.path ? ` ${leaf(frame.path)}` : " a change"}`,
        intent: "Ro Sync hit an error while applying a filesystem change.",
        target: frame.path,
      };
    case "initial-choice-needed":
      return {
        ...base,
        category: "project",
        tone: "warning",
        state: "running",
        stateLabel: "Waiting",
        title: "Choose the initial source",
        intent: "Select whether the first sync should begin from Studio or the local project.",
      };
    case "initial-choice-made":
      return {
        ...base,
        category: "project",
        tone: "success",
        title: "Initial sync source selected",
        intent: `Ro Sync will begin with ${friendlyChoice(frame.choice)}.`,
        facts: [fact("Source", friendlyChoice(frame.choice))],
      };
    case "shutdown":
      return {
        ...base,
        category: "connection",
        tone: "warning",
        state: "info",
        stateLabel: "Stopped",
        title: "Project service stopped",
        intent: "The daemon closed this project’s live connection.",
      };
    case "busy":
      return {
        ...base,
        state: "info",
        stateLabel: "Summary",
        title: "Grouped a burst of activity",
        intent: frame.count
          ? `${frame.count} rapid events were condensed to keep this view responsive.`
          : "Rapid events were condensed to keep this view responsive.",
        facts: frame.count ? [fact("Events", frame.count)] : [],
      };
    case "stream-error":
      return {
        ...base,
        category: "connection",
        tone: "danger",
        state: "failed",
        stateLabel: "Interrupted",
        title: "Activity connection interrupted",
        intent: "Ro Sync will keep trying to reconnect to this project.",
        facts: frame.attempts > 1 ? [fact("Attempts", frame.attempts)] : [],
      };
    default:
      return {
        ...base,
        title: "Project activity",
        intent: "Ro Sync reported a project status update.",
      };
  }
}

export function renderActivityFeed(container, entries, options = {}) {
  if (!container) return;
  const openKeys = new Set(options.expandedKeys || []);
  for (const key of (
    [...container.querySelectorAll("details[open][data-activity-key]")]
      .map((details) => details.dataset.activityKey)
  )) openKeys.add(key);
  const models = entries.map(formatActivity).filter(Boolean);
  if (!models.length) {
    container.replaceChildren(renderEmpty(options.emptyMessage));
    return;
  }

  const fragment = document.createDocumentFragment();
  for (const model of models) {
    fragment.appendChild(renderActivityCard(model, openKeys.has(model.key)));
  }
  container.replaceChildren(fragment);
}

export function renderActivityCard(model, expanded = false) {
  const card = document.createElement("article");
  card.className = `activity-card tone-${model.tone} state-${model.state}`;
  card.dataset.activityKey = model.key;

  const route = model.route?.from && model.route?.to
    ? `<div class="activity-route" aria-label="Moved path">
        ${copyPathButton("From", model.route.from)}
        <span class="activity-route-arrow" aria-hidden="true">→</span>
        ${copyPathButton("To", model.route.to)}
      </div>`
    : "";
  const target = model.target
    ? `<div class="activity-target">${copyPathButton("Target", model.target)}</div>`
    : "";
  const facts = model.facts?.length
    ? `<div class="activity-facts">${model.facts.map((item) => (
        `<span class="activity-fact"><span>${escapeHTML(item.label)}</span>${escapeHTML(item.value)}</span>`
      )).join("")}</div>`
    : "";
  const duration = model.duration ? `<span class="activity-duration">${escapeHTML(model.duration)}</span>` : "";
  const technical = escapeHTML(JSON.stringify(model.technical, null, 2));
  const time = formatClock(model.at);

  card.innerHTML = `
    <div class="activity-card-layout">
      <span class="activity-icon" aria-hidden="true">${categoryIcon(model.category)}</span>
      <div class="activity-card-content">
        <header class="activity-card-head">
          <span class="activity-category">${escapeHTML(categoryLabel(model.category))}</span>
          <span class="activity-card-meta">
            <span class="activity-state"><span aria-hidden="true"></span>${escapeHTML(model.stateLabel)}</span>
            ${duration}
            <time class="activity-time" datetime="${new Date(model.at).toISOString()}" title="${escapeHTML(new Date(model.at).toLocaleString())}">${escapeHTML(time)}</time>
          </span>
        </header>
        <h3 class="activity-title">${escapeHTML(model.title)}</h3>
        <p class="activity-intent">${escapeHTML(model.intent)}</p>
        ${route}${target}${facts}
        <details class="activity-technical" data-activity-key="${escapeHTML(model.key)}"${expanded ? " open" : ""}>
          <summary><span>Technical details</span><span class="activity-disclosure-icon" aria-hidden="true"></span></summary>
          <pre>${technical}</pre>
        </details>
      </div>
    </div>`;
  return card;
}

function formatCommand(entry) {
  const frame = entry.frame;
  const detail = frame.detail || {};
  const failed = frame.ok === false || frame.phase === "aborted";
  const running = frame.phase === "started";
  const base = baseModel(
    entry,
    categoryForOperation(frame.op),
    failed ? "danger" : running ? "accent" : "success",
    failed ? "Failed" : running ? "Running" : "Done",
  );
  const copy = commandCopy(frame.op, detail);
  return {
    ...base,
    state: failed ? "failed" : running ? "running" : "done",
    title: copy.title,
    intent: failed ? `${copy.intent} The action did not complete.` : copy.intent,
    target: copy.target,
    route: copy.route,
    facts: detailFacts(detail),
    duration: Number.isFinite(frame.durationMs) ? formatDuration(frame.durationMs) : "",
  };
}

function formatSync(entry) {
  const frame = entry.frame;
  const base = baseModel(entry, "sync", "success", "Done");
  const target = frame.op === "set" ? joinPath(frame.path, frame.name) : frame.path;
  if (frame.op === "rename") {
    return {
      ...base,
      title: `Renamed ${leaf(frame.from) || "a synced item"}`,
      intent: "Applied the local rename to the matching Studio instance.",
      route: { from: frame.from, to: frame.to },
      facts: compactFacts([fact("Class", frame.class)]),
    };
  }
  if (frame.op === "delete") {
    return {
      ...base,
      title: `Removed ${frame.name || leaf(frame.path) || "a synced item"}`,
      intent: "Removed the matching instance after its local file was deleted.",
      target: frame.path,
      facts: compactFacts([fact("Class", frame.class)]),
    };
  }
  if (frame.op === "class_change") {
    return {
      ...base,
      title: `Changed ${frame.name || leaf(frame.path) || "a synced item"} type`,
      intent: "Updated the Studio script class to match the local filename.",
      target: frame.to || frame.path,
      facts: compactFacts([fact("Class", frame.class)]),
    };
  }
  return {
    ...base,
    title: `Synced ${frame.name || leaf(target) || "a local change"}`,
    intent: `Created or refreshed${frame.class ? ` the ${frame.class}` : " the matching instance"} from the local project.`,
    target,
    facts: compactFacts([fact("Class", frame.class)]),
  };
}

function commandCopy(op, detail) {
  const target = detail.path || detail.focus || detail.uiTarget || detail.under || "";
  const targetName = detail.name || leaf(target) || "Studio";
  const property = friendlyIdentifier(detail.property || "");
  if (op === "get" || op === "inspect_ref") {
    return property
      ? { title: `Read ${property}`, intent: `Reads ${property} from ${targetName} without changing it.`, target }
      : { title: `Inspect ${targetName}`, intent: "Reads the selected Studio instance without changing it.", target };
  }
  if (op === "set") {
    return property
      ? { title: `Change ${property}`, intent: `Updates ${property} on ${targetName}.`, target }
      : { title: `Update ${targetName}`, intent: "Updates the selected Studio instance.", target };
  }
  if (op === "ls" || op === "tree") {
    return { title: `Browse ${targetName}`, intent: "Maps the instances beneath this Studio location.", target };
  }
  if (op === "query") {
    const selector = conciseText(detail.selector, 42);
    return {
      title: selector ? `Search for ${selector}` : "Search the DataModel",
      intent: selector
        ? "Matches the requested DataModel selector without changing Studio."
        : "Looks through Studio for instances matching the requested criteria.",
    };
  }
  if (op === "find") {
    const match = conciseText(detail.name || detail.class, 42);
    return {
      title: match ? `Find ${match}` : "Find Studio instances",
      intent: "Looks for instances matching the requested name or class.",
      target: detail.under,
    };
  }
  if (op === "find_by_attr") {
    const attribute = conciseText(detail.attribute, 42);
    return {
      title: attribute ? `Find ${attribute} attributes` : "Find attributed instances",
      intent: "Looks for instances carrying the requested attribute.",
      target: detail.under,
    };
  }
  if (op === "new") {
    const created = detail.name || detail.class || "instance";
    return { title: `Create ${created}`, intent: `Adds a new ${detail.class || "instance"} under ${targetName}.`, target };
  }
  if (op === "rm") return { title: `Remove ${targetName}`, intent: "Deletes the selected instance from Studio.", target };
  if (op === "mv") {
    return { title: `Move ${leaf(detail.from) || "Studio instance"}`, intent: "Reparents the selected instance to a new Studio location.", route: { from: detail.from, to: detail.to } };
  }
  if (op === "set_attr" || op === "rm_attr" || op === "attr_ls") {
    const verb = op === "set_attr" ? "Update" : op === "rm_attr" ? "Remove" : "Inspect";
    return { title: `${verb} ${detail.attribute || "attributes"}`, intent: `${verb}s instance metadata on ${targetName}.`, target };
  }
  if (op === "add_tag" || op === "rm_tag" || op === "tag_ls") {
    const verb = op === "add_tag" ? "Add" : op === "rm_tag" ? "Remove" : "Inspect";
    return { title: `${verb} ${detail.tag || "tags"}`, intent: `${verb}s collection tags on ${targetName}.`, target };
  }
  if (op === "call") return { title: `Call ${friendlyIdentifier(detail.method) || "Studio method"}`, intent: `Invokes a method on ${targetName}.`, target };
  if (op === "capabilities") return { title: "Check Studio capabilities", intent: "Checks which capture and playtest features the connected plugin supports." };
  if (op === "capture_status") return { title: "Check capture readiness", intent: "Checks which screenshot providers are ready to use." };
  if (op === "capture_authorize") return { title: "Authorize screen capture", intent: "Requests permission for Ro Sync to capture the Studio window." };
  if (op === "save") return { title: "Save the place", intent: "Asks Studio to save the currently open place." };
  if (op === "undo") return { title: "Undo the last change", intent: "Moves Studio change history back by one action." };
  if (op === "redo") return { title: "Redo the last change", intent: "Reapplies the next action in Studio change history." };
  if (op === "select_get") return { title: "Read Studio selection", intent: "Checks which instances are currently selected in Explorer." };
  if (op === "select_set") return { title: "Update Studio selection", intent: "Selects the requested instances in Explorer." };
  if (op === "clipboard_copy") {
    const count = Number(detail.itemCount) || 0;
    return {
      title: count ? `Copy ${count} Studio instance${count === 1 ? "" : "s"}` : "Copy Studio selection",
      intent: detail.selectionMode === "selection"
        ? "Serializes the current Explorer selection into Ro Sync’s private cross-project clipboard."
        : "Serializes the requested instance trees into Ro Sync’s private cross-project clipboard.",
    };
  }
  if (op === "clipboard_paste") {
    const count = Number(detail.itemCount) || 0;
    const target = detail.parent || "";
    return {
      title: `Paste ${count || "Studio"} instance${count === 1 ? "" : "s"}`,
      intent: target
        ? `Recreates the copied instance trees beneath ${leaf(target)} as one undoable Studio change.`
        : "Recreates the copied instance trees at their original parent paths as one undoable Studio change.",
      target,
    };
  }
  if (op.startsWith("transaction_")) return { title: `${op.endsWith("begin") ? "Begin" : "Finish"} grouped changes`, intent: "Keeps related Studio edits together as one reversible action." };
  if (op === "waypoint") return { title: "Create an undo point", intent: "Marks the current Studio state so the next edits can be reversed safely." };
  if (op === "eval") return { title: "Run a Studio check", intent: "Runs a temporary Luau check inside the plugin sandbox." };
  if (op === "capture_prepare") return { title: detail.uiTarget ? `Prepare ${leaf(detail.uiTarget)} capture` : "Prepare screen capture", intent: "Frames the requested Studio window or UI region for a screenshot.", target: detail.uiTarget || detail.focus };
  if (op === "photo_prepare") return { title: detail.focus ? `Prepare ${leaf(detail.focus)} photo` : "Prepare Studio photo", intent: "Frames and renders the requested Studio scene or UI subject.", target: detail.uiTarget || detail.focus };
  if (op === "capture_export") return { title: "Export captured image", intent: `Encodes the prepared screenshot${detail.format ? ` as ${String(detail.format).toUpperCase()}` : ""}.` };
  if (op === "transmit_prepare") return { title: "Prepare image transfer", intent: "Opens a bounded transfer for a captured image." };
  if (op.startsWith("playtest_")) return playtestCopy(op, detail);
  if (op === "logs") return { title: "Read Studio output", intent: "Collects recent messages from the Studio Output window." };
  if (op === "class_info" || op === "enum_list" || op === "enums") return { title: "Inspect Roblox types", intent: "Reads Roblox class or enum information for the requested type." };
  return { title: "Run a Studio action", intent: "Ro Sync asked Studio to complete a supported operation.", target };
}

function playtestCopy(op, detail) {
  if (op === "playtest_stop") return { title: "Stop playtest", intent: "Stops the active Studio test session." };
  if (op === "playtest_run_cancel") return { title: "Cancel playtest run", intent: "Cancels the active playscript-owned test and begins cleanup." };
  if (op === "playtest_contexts") return { title: "Inspect playtest contexts", intent: "Checks which server and client test contexts are ready." };
  if (op === "playtest_capture") return { title: `Capture ${detail.context || "playtest"}`, intent: "Takes a screenshot from the selected runtime context." };
  if (op === "playtest_request") return { title: "Check the running playtest", intent: `Runs a ${friendlyIdentifier(detail.operation) || "runtime"} check in ${detail.context || "the selected context"}.` };
  return {
    title: "Start playtest",
    intent: `Starts ${detail.mode ? `a ${detail.mode}` : "a"} Studio test${detail.players ? ` with ${detail.players} player${detail.players === 1 ? "" : "s"}` : ""}.`,
  };
}

function detailFacts(detail) {
  return compactFacts([
    fact("Property", detail.property), fact("Class", detail.class || detail.expectedClass),
    fact("Name", detail.name), fact("Selector", detail.selector),
    fact("Attribute", detail.attribute), fact("Tag", detail.tag), fact("Method", detail.method),
    fact("Mode", detail.mode), fact("Players", detail.players), fact("Context", detail.context),
    fact("Identity", detail.identity), fact("Operation", detail.operation),
    fact("View", detail.view), fact("UI", detail.ui), fact("Format", detail.format),
    fact("Depth", detail.depth), fact("Limit", detail.limit),
    fact("Enum", detail.enum), fact("Level", detail.level),
    fact("Transaction", detail.transaction), fact("Action", detail.action),
    fact("Properties", detail.propertyCount), fact("Arguments", detail.argumentCount),
    fact("Selected", detail.selectionCount), fact("Source", detail.sourceBytes != null ? formatBytes(detail.sourceBytes) : null),
    fact("Items", detail.itemCount), fact("Size", detail.byteLength != null ? formatBytes(detail.byteLength) : null),
  ]).slice(0, 6);
}

function baseModel(entry, category, tone, stateLabel) {
  const technical = scrubActivityFrame(entry.frame) || { type: "event" };
  return {
    key: entry.key,
    at: entry.at,
    category,
    tone,
    state: stateLabel === "Running" ? "running" : stateLabel === "Failed" ? "failed" : "done",
    stateLabel,
    title: "Project activity",
    intent: "Ro Sync reported a project update.",
    target: "",
    route: null,
    facts: [],
    duration: "",
    technical,
  };
}

function scrubSyncActivity(frame) {
  const op = ["set", "delete", "rename", "class_change", "update"].includes(frame.op)
    ? frame.op
    : "update";
  return {
    type: "sync-activity",
    op,
    path: safePath(frame.path),
    from: safePath(frame.from),
    to: safePath(frame.to),
    class: safeText(frame.class, 128),
    name: safeText(frame.name, 256),
  };
}

function pickPrimitiveFields(source, allowed) {
  if (!source || typeof source !== "object" || Array.isArray(source)) return {};
  const out = {};
  for (const key of allowed) {
    const value = source[key];
    if (typeof value === "string") {
      const safe = key.toLowerCase().includes("path") || ["project", "focus", "under", "from", "to", "uiTarget"].includes(key)
        ? safePath(value)
        : safeText(value, MAX_TECHNICAL_STRING);
      if (safe) out[key] = safe;
    } else if (typeof value === "boolean") {
      out[key] = value;
    } else if (typeof value === "number" && Number.isFinite(value)) {
      out[key] = value;
    } else if (Array.isArray(value) && value.length <= 64) {
      const safe = value.map((item) => safeText(item, 128)).filter(Boolean);
      if (safe.length) out[key] = safe;
    }
  }
  return out;
}

function safePath(value) {
  if (Array.isArray(value)) value = value.join("/");
  return safeText(value, 512);
}

function safeOperation(value) {
  const op = safeText(value, 64) || "unknown";
  return /^[a-z0-9_-]+$/.test(op) ? op : "unknown";
}

function safeText(value, max) {
  if (value == null) return "";
  const text = String(value).replace(/[\u0000-\u001f\u007f]/g, " ").trim();
  return text.slice(0, max);
}

function trimEntries(entries, limit) {
  const max = Math.max(1, Number(limit) || 100);
  if (entries.length > max) entries.splice(0, entries.length - max);
}

function categoryForOperation(op) {
  if (op.startsWith("playtest_")) return "playtest";
  if (op.startsWith("capture_") || op.startsWith("photo_") || op.startsWith("transmit_")) return "capture";
  if (["set", "new", "rm", "mv", "set_attr", "rm_attr", "add_tag", "rm_tag", "call", "select_set", "clipboard_paste", "eval", "transaction_begin", "transaction_finish", "waypoint", "save", "undo", "redo"].includes(op)) return "change";
  return "inspect";
}

function categoryLabel(category) {
  return ({
    inspect: "Studio read", change: "Studio change", sync: "File sync",
    playtest: "Playtest", capture: "Capture", project: "Project",
    connection: "Connection", warning: "Attention", system: "Ro Sync",
  })[category] || "Ro Sync";
}

function categoryIcon(category) {
  const attrs = 'viewBox="0 0 18 18" width="18" height="18" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"';
  const paths = {
    inspect: '<circle cx="7.7" cy="7.7" r="4.4"/><path d="m11 11 3.5 3.5"/>',
    change: '<path d="M3 13.8 4 10l7.2-7.2 3 3L7 13l-4 .8Z"/><path d="m9.8 4.2 3 3"/>',
    sync: '<path d="M3.2 6.8A6 6 0 0 1 13.8 5l1 1.7"/><path d="M14.8 3.3v3.4h-3.4"/><path d="M14.8 11.2A6 6 0 0 1 4.2 13l-1-1.7"/><path d="M3.2 14.7v-3.4h3.4"/>',
    playtest: '<path d="m6.2 4.1 7.2 4.9-7.2 4.9V4.1Z"/>',
    capture: '<path d="M3 6.2h2l1.1-1.8h5.8L13 6.2h2v7.4H3V6.2Z"/><circle cx="9" cy="9.8" r="2.3"/>',
    project: '<path d="M2.5 5.2h5l1.3 1.4h6.7v7.1h-13V5.2Z"/><path d="M2.5 5.2V3.7h4.2l1.2 1.5"/>',
    connection: '<path d="M7 6.2 5.3 4.5a2.8 2.8 0 0 0-4 4L4 11.2a2.8 2.8 0 0 0 4 0l1-1"/><path d="m11 6.8 2.7 2.7a2.8 2.8 0 0 1-4 4L8 11.8"/><path d="m6.5 11.5 5-5"/>',
    warning: '<path d="M9 2.3 16 15H2L9 2.3Z"/><path d="M9 6.7v3.8M9 13v.1"/>',
    system: '<circle cx="9" cy="9" r="6.5"/><path d="M9 8v4M9 5.5v.1"/>',
  };
  return `<svg ${attrs}>${paths[category] || paths.system}</svg>`;
}

function copyPathButton(label, path) {
  return `<button class="activity-path" type="button" data-copy-path="${escapeHTML(path)}" aria-label="Copy ${escapeHTML(label.toLowerCase())} path ${escapeHTML(path)}">
    <span class="activity-path-copy"><span class="activity-path-label">${escapeHTML(label)}</span><span class="activity-path-text">${escapeHTML(path)}</span></span>
    <svg viewBox="0 0 16 16" width="14" height="14" fill="none" stroke="currentColor" stroke-width="1.4" aria-hidden="true"><rect x="5.25" y="5.25" width="7" height="7" rx="1.25"/><path d="M10.25 5.25V3.75A1.25 1.25 0 0 0 9 2.5H3.75A1.25 1.25 0 0 0 2.5 3.75V9A1.25 1.25 0 0 0 3.75 10.25h1.5"/></svg>
  </button>`;
}

function renderEmpty(message) {
  const empty = document.createElement("div");
  empty.className = "activity-empty";
  empty.innerHTML = `${categoryIcon("system")}<strong>No activity yet</strong><span></span>`;
  empty.querySelector("span:last-child").textContent = message || "Studio and filesystem actions will appear here.";
  return empty;
}

function compactFacts(values) {
  return values.filter((item) => item && item.value !== "");
}

function fact(label, value) {
  if (value == null || value === "") return null;
  return { label, value: conciseText(value, 64) };
}

function conciseText(value, maxLength) {
  const text = safeText(value, Math.max(1, maxLength));
  if (value == null || String(value).length <= maxLength) return text;
  return `${text.slice(0, Math.max(0, maxLength - 1))}…`;
}

function friendlyChoice(value) {
  const choice = String(value || "").toLowerCase();
  if (choice.includes("studio")) return "Studio";
  if (choice.includes("disk") || choice.includes("local")) return "the local project";
  return "the selected source";
}

function friendlyIdentifier(value) {
  const text = safeText(value, 128);
  if (!text) return "";
  if (text === "CFrame") return text;
  const spaced = text
    .replace(/[_-]+/g, " ")
    .replace(/([a-z0-9])([A-Z])/g, "$1 $2")
    .replace(/\s+/g, " ")
    .toLowerCase();
  return spaced.charAt(0).toUpperCase() + spaced.slice(1);
}

function leaf(path) {
  const parts = String(path || "").split("/").filter(Boolean);
  return parts.at(-1) || "";
}

function joinPath(parent, child) {
  return [String(parent || "").replace(/\/$/, ""), String(child || "").replace(/^\//, "")]
    .filter(Boolean)
    .join("/");
}

function formatDuration(ms) {
  if (ms < 1000) return `${Math.round(ms)} ms`;
  if (ms < 60_000) return `${(ms / 1000).toFixed(ms < 10_000 ? 1 : 0)} s`;
  return `${Math.floor(ms / 60_000)}m ${Math.round((ms % 60_000) / 1000)}s`;
}

function formatBytes(bytes) {
  const value = Number(bytes) || 0;
  if (value < 1024) return `${value} B`;
  return `${(value / 1024).toFixed(1)} KB`;
}

function formatClock(ts) {
  return new Date(ts || Date.now()).toLocaleTimeString([], {
    hour: "2-digit", minute: "2-digit", second: "2-digit",
  });
}

function escapeHTML(value) {
  return String(value ?? "").replace(/[&<>"']/g, (char) => ({
    "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;",
  })[char]);
}
