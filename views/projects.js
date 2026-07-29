// views/projects.js — list+detail Projects view with sidebar-friendly shell,
// per-project controls, and a throttled activity tail for the served project.
import { daemonJson, daemonWS } from "../bridge.js";
import { copyText, joinProjectFile, pathFromDrop } from "./runtime.js";
import { pushActivity, renderActivityFeed } from "./activity-format.js";
import { formatProjectFailure } from "./project-error.js";
import {
  buildProjectionLineDiff,
  formatFileBytes,
  markerStyleLabel,
  normalizeProjectionReport,
  shortHash,
} from "./projection-repair.js";

const MAX_PROJECT_LOG_LINES = 100;
const MAX_PROJECT_PARSED_OPS_PER_SECOND = 20;
const RAW_OP_RE = /"type"\s*:\s*"op"/;
const DEFAULT_WALLY_FOLDER = "ReplicatedStorage/Assets/Packages";
// Offline repair state outlives a disposable Projects view mount. A user can
// navigate away while the non-cancellable host command is still committing;
// its eventual recovery result must still lock serving when they return.
const projectionRepairByProject = new Map();
const projectionInspectSequence = new Map();
const projectionRecoveryRequiredProjects = new Set();
const DEFAULT_WALLY_FILE = `[package]
name = "game/project"
version = "0.1.0"
registry = "https://github.com/UpliftGames/wally-index"
realm = "shared"

[dependencies]
React = "jsdotlua/react@17.1.0"
ReactRoblox = "jsdotlua/react-roblox@17.1.0"
Red = "red-blox/red@2.3.0"
Future = "red-blox/future@^1.0.0"
Guard = "red-blox/guard@1.0.1"
Promise = "evaera/promise@4.0.0"
Ratelimit = "red-blox/ratelimit@^1.0.0"
Signal = "red-blox/signal@^2.0.0"
GoodSignal = "stravant/goodsignal@0.3.1"
Trove = "sleitnick/trove@1.4.0"
Spring = "brittonfischer/spring@0.1.0"
ReactSpring = "chriscerie/react-spring@2.0.0"
Spr = "blackjackiee/spr@1.1.3"
Reflex = "littensy/reflex@4.3.1"
ReactReflex = "littensy/react-reflex@0.3.6"
Sift = "csqrl/sift@0.0.9"
Conch = "alicesaidhi/conch@0.3.1"
Conch_ui = "alicesaidhi/conch-ui@0.3.1"
Chrono = "parihsz/chrono@1.2.4"
RthroScaler = "egomoose/rthro-scaler@0.3.0"
ObjectCache = "pyseph/objectcache@1.4.6"
observers = "sleitnick/observers@0.5.0"
topbarplus = "1foreverhd/topbarplus@3.4.0"
`;

export function mountProjects(root, api) {
  root.innerHTML = `
    <div class="projects-topbar">
      <header class="page-header">
        <div class="page-titles">
          <h1 class="page-title">Projects</h1>
          <p class="page-sub">Manage your Roblox Studio projects in sync.</p>
        </div>
        <div class="page-actions">
          <button id="proj-toggle-add" class="primary" type="button">+ Add Project</button>
        </div>
      </header>

      <div id="proj-add-panel" class="add-panel" hidden>
        <div class="row">
          <input id="proj-path" class="path-input" type="text" placeholder="/absolute/path/to/project" spellcheck="false" />
          <button id="proj-pick" type="button" title="Pick folder">Browse…</button>
        </div>
        <div id="proj-path-hint" class="project-hint" hidden>
          Desktop projects must be chosen with Browse so Ro Sync can authorize that folder.
        </div>
        <div class="row">
          <input id="proj-game-id" type="text" placeholder="Game ID (optional)" spellcheck="false" inputmode="numeric" />
          <input id="proj-group-id" type="text" placeholder="Group ID (optional)" spellcheck="false" inputmode="numeric" />
          <input id="proj-place-ids" type="text" placeholder="Place IDs (comma-separated)" spellcheck="false" />
        </div>
        <div class="add-actions">
          <button id="proj-cancel-add" type="button">Cancel</button>
          <button id="proj-add" class="primary" type="button">Add Project</button>
        </div>
      </div>

      <div class="search-toolbar">
        <div class="search-wrap">
          <svg viewBox="0 0 16 16" width="14" height="14" fill="none" stroke="currentColor" stroke-width="1.5" aria-hidden="true">
            <circle cx="7.25" cy="7.25" r="4.5"/>
            <path d="m13.5 13.5-3.05-3.05" stroke-linecap="round"/>
          </svg>
          <input id="proj-search" type="search" placeholder="Search projects…" spellcheck="false" />
        </div>
        <div class="filter-pills" role="tablist" aria-label="Filter projects">
          <button class="pill" data-filter="all" aria-pressed="true">All</button>
          <button class="pill" data-filter="connected" aria-pressed="false">Serving</button>
          <button class="pill" data-filter="needs-setup" aria-pressed="false">Needs Setup</button>
        </div>
      </div>
    </div>

    <div class="projects-scroll">
      <div class="workspace" id="proj-workspace" data-mode="list">
        <div class="ws-list">
          <ul id="proj-list" class="project-list"></ul>
          <button id="proj-add-tile" class="add-tile" type="button">
            <span class="add-tile-icon">${plusSVG()}</span>
            <span class="add-tile-text">
              <span class="add-tile-title">Add your project</span>
              <span class="add-tile-sub">Click to add a Roblox Studio project</span>
            </span>
          </button>
          <div id="proj-empty" class="empty" hidden>No projects yet — click "Add Project" above to get started.</div>
          <div id="proj-empty-filter" class="empty" hidden>No projects match the current filter.</div>
        </div>
        <aside class="ws-detail" id="proj-detail" aria-label="Project details"></aside>
      </div>
    </div>
  `;

  const $addPanel = root.querySelector("#proj-add-panel");
  const $toggleAdd = root.querySelector("#proj-toggle-add");
  const $cancelAdd = root.querySelector("#proj-cancel-add");
  const $path = root.querySelector("#proj-path");
  const $pathHint = root.querySelector("#proj-path-hint");
  const $pick = root.querySelector("#proj-pick");
  const $add = root.querySelector("#proj-add");
  const $gameId = root.querySelector("#proj-game-id");
  const $groupId = root.querySelector("#proj-group-id");
  const $placeIds = root.querySelector("#proj-place-ids");
  const $list = root.querySelector("#proj-list");
  const $empty = root.querySelector("#proj-empty");
  const $emptyFilter = root.querySelector("#proj-empty-filter");
  const $addTile = root.querySelector("#proj-add-tile");
  const $search = root.querySelector("#proj-search");
  const $pills = root.querySelector(".filter-pills");
  const $workspace = root.querySelector("#proj-workspace");
  const $detail = root.querySelector("#proj-detail");

  const initialState = api.getState();
  let snapshotByProject = new Map();
  let editingId = null;
  let selectedId = initialState.activeProjectId
    || (initialState.projects && initialState.projects[0] && initialState.projects[0].id)
    || null;
  let searchQuery = "";
  let filter = "all";
  let renderedDetailProjectId = null;
  let activityWs = null;
  let activityRaf = 0;
  let activityProjectId = null;
  let activityRawWindowStart = 0;
  let activityParsedOpsInWindow = 0;
  let skippedActivityOps = 0;
  let skippedActivityTimer = 0;
  let activityErrorCount = 0;
  let lastActivityErrorAt = 0;
  let disposed = false;
  let authorizedDesktopPath = "";
  const activityFrames = [];
  const activityExpandedKeys = new Set();
  const startErrorByProject = new Map();
  const startingProjects = new Set();
  const terminalFailureProjects = new Set();
  const announcedFailureByProject = new Map();
  const openFailureDetailsByProject = new Set();

  function projectionFailureKey(failure) {
    return `${failure?.code || ""}\u0000${normalizeRepairPath(failure?.path)}`;
  }

  function clearProjectionRepair(projectId) {
    projectionRepairByProject.delete(projectId);
    projectionInspectSequence.set(
      projectId,
      (projectionInspectSequence.get(projectId) || 0) + 1,
    );
  }

  function projectionRepairLocksDaemon(projectId) {
    return projectionRepairByProject.has(projectId)
      || projectionRecoveryRequiredProjects.has(projectId);
  }

  function publishProjectionRepairState(projectId) {
    if (disposed) {
      api.emitBus?.("projection:repair-state", { projectId });
    } else {
      renderDetail();
    }
  }

  if (api.host.isDesktop) {
    $path.readOnly = true;
    $path.placeholder = "Choose a project folder with Browse…";
    $path.title = "Desktop project folders must be authorized with Browse";
    $pathHint.hidden = false;
  }

  function parsePlaceIds(raw) {
    if (!raw) return [];
    return String(raw).split(",").map((s) => s.trim()).filter(Boolean);
  }

  function isServing(projectId) {
    if (typeof api.isProjectServed === "function") return api.isProjectServed(projectId);
    return api.getState().activeProjectId === projectId;
  }

  function visibleProjects() {
    const s = api.getState();
    const all = s.projects || [];
    const q = searchQuery.trim().toLowerCase();
    return all.filter((p) => {
      if (q) {
        const hay = `${p.name || ""}\n${p.path || ""}`.toLowerCase();
        if (!hay.includes(q)) return false;
      }
      if (filter === "connected") {
        if (!isServing(p.id)) return false;
        const st = snapshotByProject.get(p.id);
        if (!st || st.ok !== true) return false;
      } else if (filter === "needs-setup") {
        if (p.gameId || p.groupId) return false;
      }
      return true;
    });
  }

  function statusFor(p) {
    const st = snapshotByProject.get(p.id) || {};
    if (startingProjects.has(p.id)) {
      return { kind: "idle", label: "Starting…", dot: "dot-idle" };
    }
    const session = api.getDaemonSession?.(p.id) || api.getState().daemonSessions?.[p.id] || null;
    const error = session?.error
      || api.getDaemonFailure?.(p.id)
      || startErrorByProject.get(p.id)
      || (st.ok === false ? st.label : "");
    if (error && st.ok !== true) {
      const failure = formatProjectFailure(error, p.path);
      return {
        kind: "err",
        label: failure.statusLabel,
        dot: "dot-err",
        detail: error,
        failure,
      };
    }
    if (!isServing(p.id)) return { kind: "idle", label: "Not serving", dot: "dot-idle" };
    if (st.ok === true) return { kind: "ok", label: "Serving", dot: "dot-ok" };
    return { kind: "idle", label: st.label || "Starting…", dot: "dot-idle" };
  }

  function renderList() {
    if (disposed) return;
    const s = api.getState();
    const allProjects = s.projects || [];
    const projects = visibleProjects();
    $list.innerHTML = "";

    if (!allProjects.length) {
      $empty.hidden = false;
      $emptyFilter.hidden = true;
      $addTile.hidden = false;
      return;
    }
    $empty.hidden = true;
    $addTile.hidden = false;

    if (!projects.length) {
      $emptyFilter.hidden = false;
      return;
    }
    $emptyFilter.hidden = true;

    for (const p of projects) {
      const li = document.createElement("li");
      li.className = "project-card";
      li.setAttribute("role", "button");
      li.setAttribute("tabindex", "0");
      if (p.id === selectedId) li.setAttribute("aria-current", "true");

      const initials = leafInitials(p.name || basename(p.path));
      const projectIsServing = isServing(p.id);
      const projectIsStarting = startingProjects.has(p.id);
      const repairIsResolving = projectionRepairByProject.get(p.id)?.status === "resolving";
      const repairBlocksStart = !projectIsServing && projectionRepairLocksDaemon(p.id);
      const st = statusFor(p);
      const dupeGroups = (snapshotByProject.get(p.id) || {}).dupeGroups || 0;

      li.innerHTML = `
        <span class="thumb">${escapeHTML(initials)}</span>
        <div class="meta">
          <span class="name"></span>
          <span class="path"></span>
        </div>
        <label class="switch toggle" title="${repairIsResolving ? "Offline repair is in progress" : (repairBlocksStart ? "Review the saved recovery receipt before serving" : (projectIsStarting ? "Daemon is starting" : (projectIsServing ? "Stop serving" : "Start serving")))}" data-act="serve-wrap">
          <input type="checkbox" data-act="serve" ${projectIsServing ? "checked" : ""} ${projectIsStarting || repairIsResolving || repairBlocksStart ? `disabled${projectIsStarting ? ' aria-busy="true"' : ""}` : ""} aria-label="Serve this project" />
          <span class="switch-track"><span class="switch-thumb"></span></span>
        </label>
        <div class="chips">
          ${p.gameId ? `<span class="chip"><span class="lbl">Game ID</span> ${escapeHTML(String(p.gameId))}</span>` : ""}
          ${p.groupId ? `<span class="chip"><span class="lbl">Group ID</span> ${escapeHTML(String(p.groupId))}</span>` : ""}
          ${p.wallyEnabled ? `<span class="chip"><span class="lbl">Wally</span> ${escapeHTML(shortStudioPath(p.wallyFolder || DEFAULT_WALLY_FOLDER))}</span>` : ""}
          <span class="chip chip-status is-${st.kind}">
            <span class="dot ${st.dot}"></span>
            ${escapeHTML(st.label)}
          </span>
          ${dupeGroups > 0 ? `<span class="chip chip-warn">${dupeGroups} duplicate-name ${dupeGroups === 1 ? "group" : "groups"}</span>` : ""}
        </div>
      `;
      li.querySelector(".name").textContent = p.name || basename(p.path);
      li.querySelector(".path").textContent = p.path;

      const $serve = li.querySelector('[data-act="serve"]');
      $serve.addEventListener("click", (e) => e.stopPropagation());
      $serve.addEventListener("change", (e) => {
        e.stopPropagation();
        if (e.target.checked) serve(p.id);
        else stopServing(p.id);
      });
      li.querySelector('[data-act="serve-wrap"]').addEventListener("click", (e) => e.stopPropagation());

      li.addEventListener("click", () => selectProject(p.id));
      li.addEventListener("keydown", (e) => {
        if (e.key === "Enter" || e.key === " ") {
          e.preventDefault();
          selectProject(p.id);
        }
      });

      $list.appendChild(li);
    }
  }

  function renderDetail() {
    if (disposed) return;
    rememberActivityExpansion();
    const priorProjectId = renderedDetailProjectId;
    const priorDetails = $detail.querySelector(".project-error-details");
    if (priorProjectId && priorDetails) {
      if (priorDetails.open) openFailureDetailsByProject.add(priorProjectId);
      else openFailureDetailsByProject.delete(priorProjectId);
    }
    const activeElement = $detail.contains(document.activeElement)
      ? document.activeElement
      : null;
    const activeDetailControl = [
      "errorAct",
      "repairAct",
      "repairChoose",
      "repairConflict",
    ].map((key) => [key, activeElement?.dataset?.[key]])
      .find(([, value]) => value != null) || null;
    const s = api.getState();
    const projects = s.projects || [];
    const p = projects.find((x) => x.id === selectedId)
      || projects.find((x) => x.id === s.activeProjectId)
      || projects[0]
      || null;

    if (!p) {
      renderedDetailProjectId = null;
      $detail.innerHTML = `
        <div class="detail-empty">
          <div class="detail-empty-icon">${folderSVG()}</div>
          <div>Select a project to see details.</div>
        </div>
      `;
      return;
    }

    if (selectedId !== p.id) selectedId = p.id;

    if (editingId === p.id) {
      renderedDetailProjectId = p.id;
      renderDetailEdit(p);
      return;
    }

    const st = statusFor(p);
    const initials = leafInitials(p.name || basename(p.path));
    const lastSync = (snapshotByProject.get(p.id) || {}).lastSync || null;
    const lastSyncLabel = formatRelative(lastSync);
    const isActive = isServing(p.id);
    const failure = st.kind === "err"
      ? (st.failure || formatProjectFailure(st.detail || st.label, p.path))
      : null;
    let projectionRepair = projectionRepairByProject.get(p.id) || null;
    const failureKey = failure ? `${failure.code}\u0000${failure.diagnostic}` : "";
    const repairFailureKey = failure ? projectionFailureKey(failure) : "";
    if (
      projectionRepair
      && !["resolving", "recovery"].includes(projectionRepair.status)
      && (!failure || projectionRepair.failureKey !== repairFailureKey)
    ) {
      clearProjectionRepair(p.id);
      projectionRepair = null;
    }
    const announceFailure = !!failure && announcedFailureByProject.get(p.id) !== failureKey;
    if (failure) announcedFailureByProject.set(p.id, failureKey);
    else announcedFailureByProject.delete(p.id);

    $detail.innerHTML = `
      <div class="detail-head">
        <button class="detail-back" type="button" data-act="back" aria-label="Back to list">${chevronLeftSVG()}</button>
        <span class="thumb">${escapeHTML(initials)}</span>
        <div class="title">
          <div class="name"></div>
          <div class="head-meta">
            <span class="pill-status is-${st.kind}"><span class="dot ${st.dot}"></span>${escapeHTML(st.label)}</span>
            ${lastSyncLabel ? `<span class="muted-sm">${escapeHTML(lastSyncLabel)}</span>` : ""}
          </div>
        </div>
        ${api.host.supports.spawnSession ? `<button class="detail-icon-btn" data-act="spawn-session" type="button" title="Spawn Session" aria-label="Spawn Session">${sessionSVG()}<span>Spawn Session</span></button>` : ""}
        <button class="detail-icon-btn" data-act="edit" type="button" title="Edit project" aria-label="Edit project">${editSVG()}<span>Edit</span></button>
      </div>
      ${failure ? projectFailureMarkup(
        failure,
        announceFailure,
        projectionRepair,
        projectionRepairLocksDaemon(p.id),
      ) : ""}
      ${failure && projectionRepair ? projectionRepairMarkup(projectionRepair, failure) : ""}
      <div class="project-log-shell">
        <div class="project-log-head">
          <span>Recent actions</span>
          <span class="muted-sm">${isActive ? "last 100" : "inactive"}</span>
        </div>
        <div class="activity-feed activity-feed--compact" data-project-log aria-live="polite" aria-label="Recent project actions"></div>
      </div>
    `;
    renderedDetailProjectId = p.id;

    $detail.querySelector(".name").textContent = p.name || basename(p.path);
    const nextDetails = $detail.querySelector(".project-error-details");
    if (nextDetails) {
      nextDetails.open = openFailureDetailsByProject.has(p.id);
      nextDetails.addEventListener("toggle", () => {
        if (nextDetails.open) openFailureDetailsByProject.add(p.id);
        else openFailureDetailsByProject.delete(p.id);
      });
    }
    if (priorProjectId === p.id && activeDetailControl) {
      const [datasetKey, value] = activeDetailControl;
      const attribute = datasetKey.replace(/[A-Z]/g, (letter) => `-${letter.toLowerCase()}`);
      const escapedValue = String(value).replace(/\\/g, "\\\\").replace(/"/g, '\\"');
      $detail.querySelector(`[data-${attribute}="${escapedValue}"]`)?.focus({ preventScroll: true });
    }

    $detail.querySelector('[data-act="back"]').addEventListener("click", () => {
      $workspace.dataset.mode = "list";
    });
    $detail.querySelector('[data-act="spawn-session"]')?.addEventListener("click", () => {
      spawnSession(p);
    });
    $detail.querySelector('[data-act="edit"]').addEventListener("click", () => {
      beginEdit(p);
    });
    $detail.querySelector('[data-error-act="copy"]')?.addEventListener("click", async (event) => {
      const button = event.currentTarget;
      try {
        await copyText(api, failure.diagnostic);
        button.textContent = "Copied";
        api.toast("Diagnostic copied");
      } catch {
        api.toast("Could not copy diagnostic");
      }
    });
    $detail.querySelector('[data-error-act="open"]')?.addEventListener("click", () => {
      openFolder(failure.path || p.path);
    });
    $detail.querySelector('[data-error-act="repair"]')?.addEventListener("click", () => {
      inspectProjectionConflicts(p, failure);
    });
    $detail.querySelector('[data-error-act="retry"]')?.addEventListener("click", async (event) => {
      const button = event.currentTarget;
      button.disabled = true;
      button.setAttribute("aria-busy", "true");
      button.textContent = "Retrying…";
      await serve(p.id, { restart: true });
    });
    bindProjectionRepairActions(p, failure);
    renderActivityLog();
  }

  function renderDetailEdit(p) {
    const s = api.getState();
    const st = statusFor(p);
    const initials = leafInitials(p.name || basename(p.path));
    const placeIdsStr = Array.isArray(p.placeIds) ? p.placeIds.join(", ") : "";
    const lastSync = (snapshotByProject.get(p.id) || {}).lastSync || null;
    const lastSyncLabel = formatRelative(lastSync);
    const summary = (snapshotByProject.get(p.id) || {}).label || (st.kind === "ok" ? "Up to date" : "—");
    const isActive = isServing(p.id);
    const daemonOk = !!api.getDaemonBase(p.id);
    const wallyEnabled = !!p.wallyEnabled;
    const wallyFolder = p.wallyFolder || (wallyEnabled ? DEFAULT_WALLY_FOLDER : "");
    const wallyFile = p.wallyFile || DEFAULT_WALLY_FILE;
    const wallyTargetLabel = wallyTomlPathPreview(p.path, wallyFolder || DEFAULT_WALLY_FOLDER);

    $detail.innerHTML = `
      <div class="detail-head">
        <button class="detail-back" type="button" data-act="back" aria-label="Back to list">${chevronLeftSVG()}</button>
        <span class="thumb">${escapeHTML(initials)}</span>
        <div class="title">
          <div class="name"></div>
          <div class="head-meta"><span class="muted-sm">Edit project</span></div>
        </div>
        <button class="detail-icon-btn" data-act="close-edit" type="button" title="Close editor" aria-label="Close editor">${xSVG()}<span>Close</span></button>
      </div>
      <div class="detail-tabbar" role="tablist" aria-label="Project detail tabs">
        <button class="detail-tab" type="button" role="tab" aria-selected="true">Edit</button>
      </div>
      <div class="detail-body">
        <div class="detail-section">
          <span class="label">Sync Status</span>
          <div class="value">${escapeHTML(summary)}${lastSyncLabel ? ` · ${escapeHTML(lastSyncLabel)}` : ""}</div>
        </div>
        <div class="detail-section">
          <span class="label">Local Path</span>
          <div class="value-row">
            <span class="value path-value"></span>
            <button data-act="open-folder" type="button">Open Folder</button>
          </div>
        </div>
        <div class="detail-section">
          <span class="label">Linked Roblox</span>
          <div class="project-edit">
            <label>Game ID
              <input type="text" data-field="gameId" spellcheck="false" inputmode="numeric" />
            </label>
            <label>Group ID
              <input type="text" data-field="groupId" spellcheck="false" inputmode="numeric" />
            </label>
            <label>Place IDs (comma-separated)
              <input type="text" data-field="placeIds" spellcheck="false" />
            </label>
            <div class="project-wally-block">
              <label class="project-toggle-row">
                <input type="checkbox" data-field="wallyEnabled" ${wallyEnabled ? "checked" : ""} />
                <span>Enable Wally</span>
              </label>
              <div class="project-wally-settings" data-wally-settings ${wallyEnabled ? "" : "hidden"}>
                <label>Required* Wally Folder
                  <input type="text" data-field="wallyFolder" spellcheck="false" placeholder="${escapeHTML(DEFAULT_WALLY_FOLDER)}" />
                </label>
                <label>Wally File
                  <textarea data-field="wallyFile" spellcheck="false" rows="16"></textarea>
                </label>
                <div class="project-hint" data-wally-target>${escapeHTML(wallyTargetLabel)}</div>
                <div class="project-wally-actions">
                  <button data-act="wally-install" type="button">Wally Install</button>
                  <span class="project-hint" data-wally-install-status></span>
                </div>
              </div>
            </div>
            <div class="row project-edit-actions">
              <button class="primary" data-act="save" type="button">Save</button>
              <button data-act="cancel" type="button">Cancel</button>
              <span class="spacer"></span>
              <button data-act="remove" class="danger" type="button">Delete project</button>
            </div>
          </div>
        </div>
        <div class="detail-section">
          <span class="label">Plugin</span>
          <div class="value">${pluginStatusLabel(isActive, daemonOk, st)}</div>
        </div>
        <div class="detail-section">
          <span class="label">Quick Actions</span>
          <div class="actions-row">
            <button data-act="snapshot" type="button" ${isActive && daemonOk ? "" : "disabled"}>Refresh Status</button>
            <button data-act="diff" type="button">View Diff</button>
          </div>
        </div>
      </div>
    `;
    $detail.querySelector(".name").textContent = p.name || basename(p.path);
    $detail.querySelector(".path-value").textContent = p.path;
    const $g = $detail.querySelector('[data-field="gameId"]');
    const $group = $detail.querySelector('[data-field="groupId"]');
    const $pl = $detail.querySelector('[data-field="placeIds"]');
    const $wallyEnabled = $detail.querySelector('[data-field="wallyEnabled"]');
    const $wallySettings = $detail.querySelector("[data-wally-settings]");
    const $wallyFolder = $detail.querySelector('[data-field="wallyFolder"]');
    const $wallyFile = $detail.querySelector('[data-field="wallyFile"]');
    const $wallyTarget = $detail.querySelector("[data-wally-target]");
    const $wallyInstall = $detail.querySelector('[data-act="wally-install"]');
    const $wallyInstallStatus = $detail.querySelector("[data-wally-install-status]");
    $g.value = p.gameId || "";
    $group.value = p.groupId || "";
    $pl.value = placeIdsStr;
    $wallyFolder.value = wallyFolder;
    $wallyFile.value = wallyFile;

    function refreshWallySettings() {
      const enabled = $wallyEnabled.checked;
      $wallySettings.hidden = !enabled;
      $wallyTarget.textContent = wallyTomlPathPreview(
        p.path,
        $wallyFolder.value,
      );
    }
    $wallyEnabled.addEventListener("change", refreshWallySettings);
    $wallyFolder.addEventListener("input", refreshWallySettings);

    $detail.querySelector('[data-act="back"]').addEventListener("click", () => {
      editingId = null;
      $workspace.dataset.mode = "list";
      renderDetail();
    });
    $detail.querySelector('[data-act="close-edit"]').addEventListener("click", () => {
      editingId = null;
      renderDetail();
    });
    $detail.querySelector('[data-act="cancel"]').addEventListener("click", () => {
      editingId = null;
      renderDetail();
    });
    $detail.querySelector('[data-act="save"]').addEventListener("click", () => {
      saveEdit(p.id, {
        gameId: $g.value.trim(),
        groupId: $group.value.trim(),
        placeIds: parsePlaceIds($pl.value),
        wallyEnabled: $wallyEnabled.checked,
        wallyFolder: $wallyFolder.value,
        wallyFile: $wallyFile.value,
      });
    });
    $wallyInstall.addEventListener("click", () => {
      installWallyFromEdit(p, {
        enabled: $wallyEnabled.checked,
        folder: $wallyFolder.value,
        file: $wallyFile.value,
        button: $wallyInstall,
        statusEl: $wallyInstallStatus,
      });
    });
    $detail.querySelector('[data-act="open-folder"]').addEventListener("click", () => openFolder(p.path));
    $detail.querySelector('[data-act="snapshot"]').addEventListener("click", () => snapshotNow(p.id));
    $detail.querySelector('[data-act="diff"]').addEventListener("click", () => {
      const tab = document.querySelector('.tab[data-route="conflicts"]');
      if (tab) tab.click();
    });

    // Two-click delete confirm — sandboxed widget can't use window.confirm.
    const $remove = $detail.querySelector('[data-act="remove"]');
    let armed = false;
    let armTimer = null;
    $remove.addEventListener("click", () => {
      if (!armed) {
        armed = true;
        $remove.textContent = "Really delete?";
        $remove.classList.add("armed");
        armTimer = setTimeout(() => {
          armed = false;
          $remove.textContent = "Delete project";
          $remove.classList.remove("armed");
        }, 4000);
        return;
      }
      clearTimeout(armTimer);
      remove(p.id);
    });
  }

  function render() {
    if (disposed) return;
    renderList();
    renderDetail();
    api.setStatus(`${(api.getState().projects || []).length} project(s)`);
    ensureActivityStream();
  }

  function selectProject(id) {
    selectedId = id;
    if (api.host.supports.multiDaemon) api.setState({ activeProjectId: id });
    editingId = null;
    $workspace.dataset.mode = "detail";
    renderList();
    renderDetail();
  }

  async function beginEdit(p) {
    editingId = p.id;
    renderDetail();
    try {
      await hydrateProjectConfig(p.id);
      renderDetail();
    } catch (e) {
      console.warn("hydrate project config failed", e);
    }
  }

  async function saveEdit(id, form) {
    const s = api.getState();
    const prev = (s.projects || []).find((p) => p.id === id);
    if (!prev) return;
    const placeIds = Array.isArray(form.placeIds) ? form.placeIds : [];
    const prevGameId = (prev && prev.gameId) || null;
    const prevGroupId = (prev && prev.groupId) || null;
    const prevPlaceIds = (prev && Array.isArray(prev.placeIds)) ? prev.placeIds.join(",") : "";
    const nextGameId = form.gameId || null;
    const nextGroupId = form.groupId || null;
    const nextPlaceIdsStr = placeIds.join(",");
    const nextWallyEnabled = !!form.wallyEnabled;
    let nextWallyFolder = "";
    try {
      if (nextWallyEnabled) nextWallyFolder = validateWallyFolder(form.wallyFolder);
    } catch (error) {
      api.toast(error.message);
      const input = $detail.querySelector('[data-field="wallyFolder"]');
      if (input) input.focus();
      return;
    }
    const nextWallyFile = String(form.wallyFile || "").trimEnd() + "\n";

    if (nextWallyEnabled && !nextWallyFolder) {
      api.toast("Wally folder is required");
      const input = $detail.querySelector('[data-field="wallyFolder"]');
      if (input) input.focus();
      return;
    }
    if (nextWallyEnabled && !nextWallyFile.trim()) {
      api.toast("Wally file is required");
      const input = $detail.querySelector('[data-field="wallyFile"]');
      if (input) input.focus();
      return;
    }

    const changedLaunchArgs =
      (prevGameId !== nextGameId) ||
      (prevGroupId !== nextGroupId) ||
      (prevPlaceIds !== nextPlaceIdsStr);

    const next = (s.projects || []).map((p) =>
      p.id === id ? {
        ...p,
        gameId: nextGameId,
        groupId: nextGroupId,
        placeIds,
        wallyEnabled: nextWallyEnabled,
        wallyFolder: nextWallyEnabled ? nextWallyFolder : "",
        wallyFile: nextWallyEnabled ? nextWallyFile : "",
      } : p
    );
    api.setState({ projects: next });

    try {
      await saveProjectConfig(next.find((p) => p.id === id));
    } catch (e) {
      console.warn("save project config failed", e);
      api.toast(`ro-sync.json write failed: ${e.message}`);
      return;
    }

    editingId = null;
    render();

    if (isServing(id) && changedLaunchArgs) {
      if (typeof api.restartProject === "function") {
        try { await api.restartProject(id); } catch (e) { console.warn("restartProject", e); }
      } else {
        if (typeof api.killDaemon === "function") {
          try { await api.killDaemon(id); } catch (e) { console.warn("killDaemon", e); }
        }
        if (typeof api.ensureDaemon === "function") {
          try { await api.ensureDaemon(id); } catch (e) { console.warn("ensureDaemon", e); }
        }
      }
      api.toast("Saved — daemon restarted");
    } else {
      api.toast("Saved");
    }
  }

  async function add() {
    const decision = assessProjectPath({
      isDesktop: api.host.isDesktop,
      source: "manual",
      path: $path.value,
      authorizedPath: authorizedDesktopPath,
    });
    if (!decision.ok) {
      api.toast(decision.message);
      (api.host.isDesktop ? $pick : $path).focus();
      return;
    }
    const path = decision.path;
    const s = api.getState();
    if ((s.projects || []).some((p) => p.path === path)) {
      api.toast("Project already added");
      return;
    }
    const proj = {
      id: "p_" + Date.now().toString(36) + Math.random().toString(36).slice(2, 6),
      name: basename(path),
      path,
      addedAt: Date.now(),
      gameId: $gameId.value.trim() || null,
      groupId: $groupId.value.trim() || null,
      placeIds: parsePlaceIds($placeIds.value),
      wallyEnabled: false,
      wallyFolder: "",
      wallyFile: "",
    };
    const next = [...(s.projects || []), proj];
    api.setState({ projects: next });
    selectedId = proj.id;
    $path.value = "";
    authorizedDesktopPath = "";
    $gameId.value = "";
    $groupId.value = "";
    $placeIds.value = "";
    closeAddPanel();
    render();
    refreshStatuses();
  }

  async function remove(id) {
    const s = api.getState();
    const wasServing = isServing(id);
    if (wasServing) {
      try {
        const stopped = typeof api.stopProject === "function"
          ? await api.stopProject(id)
          : (typeof api.killDaemon === "function" ? await api.killDaemon(id) : true);
        if (stopped === false) {
          api.toast("Could not stop this project; it was not removed");
          return;
        }
      } catch (e) {
        console.warn("stopProject", e);
        api.toast("Could not stop this project; it was not removed");
        return;
      }
    }
    const next = (s.projects || []).filter((p) => p.id !== id);
    api.setState({
      projects: next,
      activeProjectId: s.activeProjectId === id ? null : s.activeProjectId,
      servedProjectIds: (api.getState().servedProjectIds || []).filter((projectId) => projectId !== id),
    });
    snapshotByProject.delete(id);
    startErrorByProject.delete(id);
    terminalFailureProjects.delete(id);
    api.reportDaemonFailure?.(id, null);
    announcedFailureByProject.delete(id);
    openFailureDetailsByProject.delete(id);
    projectionRepairByProject.delete(id);
    projectionInspectSequence.delete(id);
    projectionRecoveryRequiredProjects.delete(id);
    if (editingId === id) editingId = null;
    if (selectedId === id) selectedId = (next[0] && next[0].id) || null;
    $workspace.dataset.mode = "list";
    render();
  }

  async function serve(id, { restart = false } = {}) {
    if (startingProjects.has(id)) return;
    if (projectionRepairLocksDaemon(id)) {
      api.toast("Finish or review the offline repair before starting the daemon.");
      render();
      return;
    }
    startingProjects.add(id);
    api.setState({ activeProjectId: id });
    selectedId = id;
    startErrorByProject.delete(id);
    terminalFailureProjects.delete(id);
    api.reportDaemonFailure?.(id, null);
    render();
    try {
      let started = true;
      if (restart && isServing(id) && typeof api.restartProject === "function") {
        started = await api.restartProject(id);
      } else if (typeof api.serveProject === "function") started = await api.serveProject(id);
      else {
        api.setState({ activeProjectId: id });
        await api.ensureDaemon?.(id);
      }
      if (started === false) {
        const session = api.getDaemonSession?.(id) || api.getState().daemonSessions?.[id] || null;
        const diagnostic = session?.error
          || api.getDaemonFailure?.(id)
          || "The daemon did not become ready. Open Details for the startup diagnostic.";
        startErrorByProject.set(id, diagnostic);
        api.reportDaemonFailure?.(id, diagnostic);
        api.toast("Daemon could not start. Review the project error for the next step.");
      }
    } catch (error) {
      console.warn("serveProject", error);
      const diagnostic = error?.message || String(error);
      startErrorByProject.set(id, diagnostic);
      api.reportDaemonFailure?.(id, diagnostic);
      api.toast("Daemon could not start. Review the project error for the next step.");
    } finally {
      try {
        await refreshStatuses();
      } finally {
        startingProjects.delete(id);
        render();
      }
    }
  }

  async function inspectProjectionConflicts(project, failure, notice = "") {
    if (projectionRepairByProject.get(project.id)?.status === "resolving") return;
    const failureKey = projectionFailureKey(failure);
    const sequence = (projectionInspectSequence.get(project.id) || 0) + 1;
    projectionInspectSequence.set(project.id, sequence);
    projectionRepairByProject.set(project.id, {
      status: "loading",
      report: null,
      currentId: "",
      selectedKeep: "",
      error: "",
      notice,
      failureKey,
    });
    renderDetail();
    try {
      if (typeof api.suspendProjectForRepair !== "function") {
        throw new Error("This Ro Sync host cannot safely pause automatic daemon retries.");
      }
      const suspended = await api.suspendProjectForRepair(project.id);
      if (!suspended) {
        throw new Error("Ro Sync could not pause automatic daemon retries for this project.");
      }
      const raw = await api.host.projectionInspect(project.path);
      if (projectionInspectSequence.get(project.id) !== sequence) return;
      const report = normalizeProjectionReport(raw);
      if (!report.ok) {
        if (report.resolution?.recoveryRequired) {
          projectionRecoveryRequiredProjects.add(project.id);
          projectionRepairByProject.set(project.id, {
            status: "recovery",
            report,
            currentId: "",
            selectedKeep: "",
            error: report.resolution.recoveryError
              || report.error
              || "Ro Sync found a prior repair that still requires recovery.",
            notice: "",
            failureKey,
          });
        } else {
          projectionRepairByProject.set(project.id, {
            status: "error",
            report,
            currentId: "",
            selectedKeep: "",
            error: report.error || "Ro Sync could not inspect this projection.",
            notice: "",
            failureKey,
          });
        }
      } else {
        const current = chooseProjectionConflict(report, failure);
        if (
          report.countsKnown
          && !report.truncated
          && report.conflicts.length === 0
          && report.remaining === 0
          && report.totalConflicts === 0
        ) {
          projectionRecoveryRequiredProjects.delete(project.id);
        }
        projectionRepairByProject.set(project.id, {
          status: "ready",
          report,
          currentId: current?.id || "",
          selectedKeep: "",
          error: "",
          notice,
          failureKey,
        });
      }
    } catch (error) {
      if (projectionInspectSequence.get(project.id) !== sequence) return;
      projectionRepairByProject.set(project.id, {
        status: "error",
        report: null,
        currentId: "",
        selectedKeep: "",
        error: error?.message || String(error),
        notice: "",
        failureKey,
      });
    }
    if (projectionInspectSequence.get(project.id) === sequence) {
      publishProjectionRepairState(project.id);
    }
  }

  async function resolveProjectionConflict(project, failure, conflict, keep) {
    const currentState = projectionRepairByProject.get(project.id);
    if (!currentState || currentState.status === "resolving") return;
    const sequence = (projectionInspectSequence.get(project.id) || 0) + 1;
    projectionInspectSequence.set(project.id, sequence);
    projectionRepairByProject.set(project.id, {
      ...currentState,
      status: "resolving",
      error: "",
    });
    renderDetail();
    try {
      const raw = await api.host.projectionResolve(project.path, conflict.id, keep || null);
      if (projectionInspectSequence.get(project.id) !== sequence) return;
      const report = normalizeProjectionReport(raw);
      if (!report.ok) {
        if (report.resolution?.recoveryRequired) {
          projectionRecoveryRequiredProjects.add(project.id);
          projectionRepairByProject.set(project.id, {
            status: "recovery",
            report,
            currentId: "",
            selectedKeep: "",
            error: report.resolution.recoveryError
              || report.error
              || "Ro Sync preserved a recovery receipt because the repair could not be proven complete.",
            notice: "",
            failureKey: currentState.failureKey,
          });
          api.toast("Repair paused safely. Review the recovery receipt before retrying the daemon.");
          publishProjectionRepairState(project.id);
          return;
        }
        const stale = /stale|changed|no longer|not found/i.test(
          `${report.code} ${report.error}`,
        );
        if (stale) {
          if (disposed) {
            projectionRepairByProject.set(project.id, {
              ...currentState,
              status: "error",
              selectedKeep: "",
              error: "The files changed while this review was open. Inspect them again before resolving.",
            });
            publishProjectionRepairState(project.id);
            return;
          }
          projectionRepairByProject.set(project.id, {
            ...currentState,
            status: "ready",
            selectedKeep: "",
            error: "",
          });
          await inspectProjectionConflicts(
            project,
            failure,
            "The files changed while this review was open, so Ro Sync refreshed them without moving anything.",
          );
          return;
        }
        const preserveRecovery = currentState.status === "recovery";
        projectionRepairByProject.set(project.id, {
          ...currentState,
          status: preserveRecovery ? "recovery" : "ready",
          report: currentState.report,
          selectedKeep: "",
          confirmQuarantine: false,
          error: report.error || "Ro Sync could not complete the repair.",
        });
        publishProjectionRepairState(project.id);
        return;
      }

      const archived = report.resolution?.backupPaths?.length || 0;
      const resumedRecovery = currentState.status === "recovery";
      const resolutionNotice = resumedRecovery
        ? keep === "quarantine"
          ? "The broken recovery record was quarantined. Source files were not changed by that action."
          : "The durable recovery decision was replayed and verified."
        : archived > 0
          ? `${archived === 1 ? "The other file was" : `${archived} files were`} archived under .rosync-backups. Nothing was deleted.`
          : "The filename was migrated without changing the script source.";
      const projectionIsProvenClean = report.countsKnown
        && !report.truncated
        && report.conflicts.length === 0
        && report.remaining === 0
        && report.totalConflicts === 0;
      if (projectionIsProvenClean) {
        projectionRecoveryRequiredProjects.delete(project.id);
        clearProjectionRepair(project.id);
        api.toast(`${resolutionNotice} Retrying the daemon…`);
        await serve(project.id, { restart: true });
        return;
      }

      const next = chooseProjectionConflict(report, failure);
      projectionRepairByProject.set(project.id, {
        status: "ready",
        report,
        currentId: next?.id || "",
        selectedKeep: "",
        error: "",
        notice: resolutionNotice,
        failureKey: currentState.failureKey,
      });
      api.toast(`Resolved. ${report.remaining} startup ${report.remaining === 1 ? "blocker remains" : "blockers remain"}.`);
      publishProjectionRepairState(project.id);
    } catch (error) {
      if (projectionInspectSequence.get(project.id) !== sequence) return;
      const message = error?.message || String(error);
      projectionRecoveryRequiredProjects.add(project.id);
      projectionRepairByProject.set(project.id, {
        status: "recovery",
        report: {
          ok: false,
          code: "PROJECTION_RESULT_UNVERIFIED",
          error: message,
          conflicts: [],
          remaining: 0,
          totalConflicts: 0,
          countsKnown: false,
          truncated: true,
          resolution: {
            receiptPath: "",
            receiptAvailable: false,
            recoveryRequired: true,
            recoveryError: message,
          },
        },
        currentId: "",
        selectedKeep: "",
        error: `Ro Sync lost the final repair result: ${message}`,
        notice: "",
        failureKey: currentState.failureKey,
      });
      api.toast("Repair outcome is unverified. Automatic daemon retries remain paused.");
      publishProjectionRepairState(project.id);
    }
  }

  function bindProjectionRepairActions(project, failure) {
    const state = projectionRepairByProject.get(project.id);
    if (!state) return;
    const conflict = currentProjectionConflict(state, failure);

    $detail.querySelector('[data-repair-act="refresh"]')?.addEventListener("click", () => {
      inspectProjectionConflicts(project, failure);
    });
    $detail.querySelector('[data-repair-act="close"]')?.addEventListener("click", () => {
      if (state.status === "resolving") return;
      clearProjectionRepair(project.id);
      renderDetail();
    });
    $detail.querySelector('[data-repair-act="open-backup"]')?.addEventListener("click", () => {
      openFolder(joinProjectFile(project.path, ".rosync-backups"));
    });
    $detail.querySelector('[data-repair-act="resume-recovery"]')?.addEventListener("click", () => {
      const recoveryId = state.report?.resolution?.id;
      if (!recoveryId || state.status === "resolving") return;
      resolveProjectionConflict(project, failure, { id: recoveryId }, null);
    });
    $detail.querySelector('[data-repair-act="quarantine-recovery"]')?.addEventListener("click", () => {
      if (state.status === "resolving") return;
      projectionRepairByProject.set(project.id, {
        ...state,
        confirmQuarantine: true,
      });
      renderDetail();
    });
    $detail.querySelector('[data-repair-act="cancel-quarantine"]')?.addEventListener("click", () => {
      projectionRepairByProject.set(project.id, {
        ...state,
        confirmQuarantine: false,
      });
      renderDetail();
    });
    $detail.querySelector('[data-repair-act="confirm-quarantine"]')?.addEventListener("click", () => {
      const recoveryId = state.report?.resolution?.id;
      if (!recoveryId || state.status === "resolving") return;
      resolveProjectionConflict(project, failure, { id: recoveryId }, "quarantine");
    });
    $detail.querySelectorAll("[data-repair-conflict]").forEach((button) => {
      button.addEventListener("click", () => {
        if (state.status === "resolving") return;
        const next = state.report?.conflicts?.[Number(button.dataset.repairConflict)];
        if (!next) return;
        projectionRepairByProject.set(project.id, {
          ...state,
          currentId: next.id,
          selectedKeep: "",
          error: "",
        });
        renderDetail();
      });
    });
    $detail.querySelectorAll("[data-repair-choose]").forEach((button) => {
      button.addEventListener("click", () => {
        if (state.status === "resolving") return;
        const file = conflict?.files?.[Number(button.dataset.repairChoose)];
        if (!file) return;
        projectionRepairByProject.set(project.id, {
          ...state,
          selectedKeep: file.name,
          error: "",
        });
        renderDetail();
      });
    });
    $detail.querySelector('[data-repair-act="cancel-choice"]')?.addEventListener("click", () => {
      if (state.status === "resolving") return;
      projectionRepairByProject.set(project.id, {
        ...state,
        selectedKeep: "",
        error: "",
      });
      renderDetail();
    });
    $detail.querySelector('[data-repair-act="confirm"]')?.addEventListener("click", () => {
      if (!conflict || !state.selectedKeep) return;
      resolveProjectionConflict(project, failure, conflict, state.selectedKeep);
    });
    $detail.querySelector('[data-repair-act="migrate"]')?.addEventListener("click", () => {
      if (!conflict) return;
      resolveProjectionConflict(project, failure, conflict, conflict.files?.[0]?.name || null);
    });
  }

  async function stopServing(id) {
    if (!isServing(id)) { render(); return; }
    if (projectionRepairByProject.get(id)?.status === "resolving") {
      api.toast("Wait for the offline repair to finish before changing daemon state.");
      return;
    }
    try {
      if (typeof api.stopProject === "function") await api.stopProject(id);
      else if (typeof api.killDaemon === "function") await api.killDaemon(id);
    } catch (e) {
      console.warn("stopProject", e);
      api.toast(`Could not stop project: ${e.message}`);
    }
    startErrorByProject.delete(id);
    terminalFailureProjects.delete(id);
    api.reportDaemonFailure?.(id, null);
    clearProjectionRepair(id);
    render();
    await refreshStatuses();
  }

  async function pickFolder() {
    try {
      const raw = String(await api.host.pickFolder("Pick Ro Sync project folder") || "")
        .replace(/^﻿/, "")
        .trim();
      const out = trimProjectPath(raw);
      const looksLikePath =
        out && !/\r?\n/.test(out) && /^(?:[A-Za-z]:[\\/]|[\\/]|~)/.test(out);
      if (looksLikePath) {
        $path.value = out;
        if (api.host.isDesktop) authorizedDesktopPath = out;
      } else if (!out) {
        // user cancelled — silent
      } else {
        api.toast("Folder picker failed");
        console.warn("pickFolder: ignoring non-path result", { raw });
      }
    } catch (e) {
      api.toast("Folder picker unavailable");
    }
  }

  async function openFolder(p) {
    try {
      await api.host.openPath(p);
    } catch (e) {
      api.toast("Open folder failed");
    }
  }

  async function hydrateProjectConfig(id) {
    const s = api.getState();
    const proj = (s.projects || []).find((p) => p.id === id);
    if (!proj) return;

    let cfg = {};
    try {
      const raw = await readTextFile(api, joinProjectFile(proj.path, "ro-sync.json"));
      if (raw && raw.trim()) cfg = JSON.parse(raw);
    } catch {
      cfg = {};
    }

    const wallyEnabled = Boolean(cfg.wallyEnabled ?? proj.wallyEnabled);
    const wallyFolder = String(cfg.wallyFolder ?? proj.wallyFolder ?? "");
    let validatedWallyFolder = "";
    try {
      validatedWallyFolder = validateWallyFolder(wallyFolder || DEFAULT_WALLY_FOLDER);
    } catch {
      // Preserve invalid persisted input for correction, but never turn it
      // into a filesystem path or Wally working directory.
    }
    let wallyFile = typeof cfg.wallyFile === "string" ? cfg.wallyFile : (proj.wallyFile || "");
    if (wallyEnabled && validatedWallyFolder && !wallyFile) {
      try {
        wallyFile = await readTextFile(api, wallyTomlPathForFolder(proj.path, validatedWallyFolder));
      } catch {
        wallyFile = "";
      }
    }
    if (wallyEnabled && !wallyFile) wallyFile = DEFAULT_WALLY_FILE;

    const next = (s.projects || []).map((p) => {
      if (p.id !== id) return p;
      return {
        ...p,
        gameId: cfg.gameId ?? p.gameId ?? null,
        groupId: cfg.groupId ?? p.groupId ?? null,
        placeIds: Array.isArray(cfg.placeIds) ? cfg.placeIds : (Array.isArray(p.placeIds) ? p.placeIds : []),
        wallyEnabled,
        wallyFolder,
        wallyFile,
      };
    });
    api.setState({ projects: next });
  }

  async function saveProjectConfig(proj) {
    if (!proj) return;
    const cfgPath = joinProjectFile(proj.path, "ro-sync.json");
    let existing = {};
    try {
      const raw = await readTextFile(api, cfgPath);
      if (raw && raw.trim()) existing = JSON.parse(raw);
    } catch {
      existing = {};
    }

    const merged = {
      ...existing,
      name: existing.name || proj.name || basename(proj.path),
      gameId: proj.gameId || null,
      groupId: proj.groupId || null,
      placeIds: Array.isArray(proj.placeIds) ? proj.placeIds : [],
      wallyEnabled: !!proj.wallyEnabled,
      version: existing.version || 1,
    };

    if (proj.wallyEnabled) {
      merged.wallyFolder = validateWallyFolder(proj.wallyFolder || DEFAULT_WALLY_FOLDER);
      merged.wallyFile = String(proj.wallyFile || DEFAULT_WALLY_FILE).trimEnd() + "\n";
      const tomlPath = wallyTomlPathForFolder(proj.path, merged.wallyFolder);
      await writeTextFile(api, tomlPath, merged.wallyFile);
    } else {
      delete merged.wallyFolder;
      delete merged.wallyFile;
    }

    await writeTextFile(api, cfgPath, JSON.stringify(merged, null, 2) + "\n");
  }

  async function installWallyFromEdit(proj, form) {
    if (!form.enabled) {
      api.toast("Enable Wally first");
      return;
    }

    let folder = "";
    try {
      folder = validateWallyFolder(form.folder);
    } catch (error) {
      api.toast(error.message);
      const input = $detail.querySelector('[data-field="wallyFolder"]');
      if (input) input.focus();
      return;
    }
    const file = String(form.file || "").trimEnd() + "\n";
    if (!folder) {
      api.toast("Wally folder is required");
      const input = $detail.querySelector('[data-field="wallyFolder"]');
      if (input) input.focus();
      return;
    }
    if (!file.trim()) {
      api.toast("Wally file is required");
      const input = $detail.querySelector('[data-field="wallyFile"]');
      if (input) input.focus();
      return;
    }

    const tomlPath = wallyTomlPathForFolder(proj.path, folder);
    const cwd = dirnamePath(tomlPath);
    const button = form.button;
    const statusEl = form.statusEl;
    if (button) button.disabled = true;
    if (statusEl) statusEl.textContent = "Saving Wally config...";
    const s = api.getState();
    const wasActive = isServing(proj.id);

    try {
      const nextProj = { ...proj, wallyEnabled: true, wallyFolder: folder, wallyFile: file };
      api.setState({
        projects: (s.projects || []).map((p) => p.id === proj.id ? nextProj : p),
      });
      await saveProjectConfig(nextProj);
      if (statusEl) statusEl.textContent = "Running wally install...";
      const res = await api.host.wallyInstall(cwd);
      if (res && res.code !== 0 && res.code != null) {
        throw new Error((res.stderr || res.stdout || `exit ${res.code}`).trim());
      }
      const summary = summarizeWallyInstall(res);
      if (statusEl) statusEl.textContent = wasActive
        ? `${summary}; syncing packages`
        : summary;
      api.toast(summary);
    } catch (e) {
      const msg = `Wally install failed: ${e.message}`;
      if (statusEl) statusEl.textContent = msg;
      api.toast(msg);
    } finally {
      if (button) button.disabled = false;
    }
  }

  async function snapshotNow(id) {
    const base = api.getDaemonBase(id);
    if (!isServing(id) || !base) {
      api.toast("Start serving this project first");
      return;
    }
    api.toast("Refreshing status…");
    await refreshStatuses();
    api.toast("Status refreshed");
  }

  async function spawnSession(p) {
    const payload = {
      cwd: p.path,
      name: `${p.name || basename(p.path)} Session`,
    };

    try {
      const bounds = await api.host.windowBounds();
      const x = Number(bounds && bounds.x);
      const y = Number(bounds && bounds.y);
      const width = Number(bounds && bounds.width);
      if (Number.isFinite(x) && Number.isFinite(y) && Number.isFinite(width)) {
        payload.x = Math.round(x + width + 20);
        payload.y = Math.round(y);
      }
    } catch {
      // Older hosts may not support bounds; create-session will use its default placement.
    }

    try {
      const res = await api.host.createSession(payload);
      if (res && res.supported === false) {
        api.toast("Terminal sessions are available in the Terminal 64 widget");
        return;
      }
      if (res && res.error) throw new Error(res.error);
      api.toast("Session spawned");
    } catch (e) {
      console.warn("spawn session failed", e);
      api.toast("Spawn session failed");
    }
  }

  async function refreshStatuses() {
    if (disposed) return;
    const s = api.getState();
    for (const p of s.projects || []) {
      if (disposed) return;
      if (!isServing(p.id)) {
        snapshotByProject.set(p.id, { ok: null, label: "inactive" });
        continue;
      }
      const base = api.getDaemonBase(p.id);
      if (!base) {
        const failure = api.getDaemonFailure?.(p.id) || startErrorByProject.get(p.id) || "";
        snapshotByProject.set(p.id, {
          ok: failure ? false : null,
          label: failure || "daemon offline",
        });
        continue;
      }
      try {
        // Project cards only need daemon/plugin health. Avoid `/snapshot`
        // here: it serializes the complete filesystem projection and made a
        // simple status refresh scale with the size of the entire place.
        const info = await daemonJson(base, "/hello");
        if (disposed) return;
        const failure = api.getDaemonFailure?.(p.id) || startErrorByProject.get(p.id) || "";
        if (terminalFailureProjects.has(p.id) || failure) {
          snapshotByProject.set(p.id, {
            ok: false,
            label: failure || "Sync stopped; action required",
            dupeGroups: 0,
          });
          continue;
        }
        snapshotByProject.set(p.id, {
          ok: true,
          label: info.pluginConnected ? "Studio connected" : "Waiting for Studio",
          dupeGroups: 0,
        });
        startErrorByProject.delete(p.id);
      } catch (e) {
        snapshotByProject.set(p.id, { ok: false, label: e.message });
      }
    }
    render();
  }

  function ensureActivityStream() {
    if (disposed) {
      closeActivityStream();
      return;
    }
    const nextProjectId = selectedId && isServing(selectedId) ? selectedId : null;
    if (nextProjectId !== activityProjectId) {
      closeActivityStream();
      activityProjectId = nextProjectId;
      activityFrames.length = 0;
      activityExpandedKeys.clear();
      activityRawWindowStart = 0;
      activityParsedOpsInWindow = 0;
      skippedActivityOps = 0;
      scheduleActivityRender();
    }

    const base = api.getDaemonBase(activityProjectId);
    if (!base || !activityProjectId) {
      closeActivityStream();
      return;
    }
    if (activityWs) return;

    try {
      activityWs = daemonWS(base, "/ws", {
        skipRaw: shouldSkipRawActivityFrame,
        open: () => {
          activityErrorCount = 0;
          lastActivityErrorAt = 0;
          void refreshStatuses();
        },
        error: () => {
          pushActivityStreamError();
        },
        close: () => {},
        message: (data) => {
          if (!data || typeof data !== "object") return;
          const t = data.type;
          if (t === "ping" || t === "pong" || t === "lagged"
              || t === "push-result" || t === "error") return;
          if (t === "plugin") {
            pushActivityFrame(data);
            handlePluginState(data, activityProjectId);
            return;
          }
          if (t === "shutdown") {
            pushActivityFrame(data);
            handleRuntimeShutdown(data, activityProjectId);
            return;
          }
          pushActivityFrame(data);
        },
      });
    } catch (e) {
      pushActivityFrame({ type: "stream-error", attempts: 1 });
    }
  }

  function pushActivityStreamError() {
    const now = Date.now();
    activityErrorCount++;
    if (lastActivityErrorAt && now - lastActivityErrorAt < 30_000) return;
    const count = activityErrorCount;
    activityErrorCount = 0;
    lastActivityErrorAt = now;
    pushActivityFrame({
      type: "stream-error",
      attempts: count,
    });
  }

  function shouldSkipRawActivityFrame(raw) {
    if (typeof raw !== "string" || !RAW_OP_RE.test(raw)) return false;
    if (document.hidden) return true;

    const now = Date.now();
    if (now - activityRawWindowStart >= 1000) {
      activityRawWindowStart = now;
      activityParsedOpsInWindow = 0;
      flushSkippedActivityNotice();
    }
    if (activityParsedOpsInWindow < MAX_PROJECT_PARSED_OPS_PER_SECOND) {
      activityParsedOpsInWindow++;
      return false;
    }

    skippedActivityOps++;
    scheduleSkippedActivityNotice();
    return true;
  }

  function scheduleSkippedActivityNotice() {
    if (skippedActivityTimer) return;
    skippedActivityTimer = setTimeout(flushSkippedActivityNotice, 1000);
  }

  function flushSkippedActivityNotice() {
    if (skippedActivityTimer) {
      clearTimeout(skippedActivityTimer);
      skippedActivityTimer = 0;
    }
    if (skippedActivityOps <= 0) return;
    const count = skippedActivityOps;
    skippedActivityOps = 0;
    pushActivityFrame({
      type: "busy",
      count,
    });
  }

  function closeActivityStream() {
    if (!activityWs) return;
    const ws = activityWs;
    activityWs = null;
    try { ws.close(); } catch {}
  }

  function runtimeShutdownDiagnostic(frame) {
    const code = typeof frame?.code === "string" && frame.code.trim()
      ? `${frame.code.trim()}: `
      : "";
    const reason = typeof frame?.reason === "string" && frame.reason.trim()
      ? frame.reason.trim()
      : "Ro Sync stopped this project connection.";
    return `Error: ${code}${reason}`;
  }

  function handleRuntimeShutdown(frame, fallbackProjectId = null) {
    const projectId = frame?.projectId
      || (frame?.projectPath
        && (api.getState().projects || []).find((project) => project.path === frame.projectPath)?.id)
      || fallbackProjectId;
    if (!projectId) return;
    if (frame.retryable === false) {
      const diagnostic = runtimeShutdownDiagnostic(frame);
      terminalFailureProjects.add(projectId);
      startErrorByProject.set(projectId, diagnostic);
      api.reportDaemonFailure?.(projectId, diagnostic);
      snapshotByProject.set(projectId, { ok: false, label: diagnostic });
    } else if (frame.retryable === true) {
      terminalFailureProjects.delete(projectId);
      snapshotByProject.set(projectId, { ok: null, label: "Reconnecting…" });
    }
    render();
  }

  function handlePluginState(frame, fallbackProjectId = null) {
    if (frame?.connected !== true) return;
    const projectId = frame.projectId
      || (frame.projectPath
        && (api.getState().projects || []).find((project) => project.path === frame.projectPath)?.id)
      || fallbackProjectId;
    if (!projectId) return;
    terminalFailureProjects.delete(projectId);
    startErrorByProject.delete(projectId);
    api.reportDaemonFailure?.(projectId, null);
    void refreshStatuses();
  }

  function pushActivityFrame(frame) {
    if (disposed) return;
    pushActivity(activityFrames, frame, Date.now(), MAX_PROJECT_LOG_LINES);
    scheduleActivityRender();
  }

  function scheduleActivityRender() {
    if (activityRaf) return;
    activityRaf = requestAnimationFrame(() => {
      activityRaf = 0;
      renderActivityLog();
    });
  }

  function renderActivityLog() {
    const $log = $detail.querySelector("[data-project-log]");
    if (!$log) return;
    const selectedIsActive = selectedId && isServing(selectedId);
    if (!selectedIsActive) {
      renderActivityFeed($log, [], {
        emptyMessage: "Start serving this project to see its actions.",
        expandedKeys: activityExpandedKeys,
      });
      return;
    }
    if (!activityFrames.length) {
      renderActivityFeed($log, [], {
        emptyMessage: "Studio and filesystem actions will appear here.",
        expandedKeys: activityExpandedKeys,
      });
      return;
    }

    const stickToBottom = $log.scrollHeight - $log.scrollTop - $log.clientHeight < 32;
    renderActivityFeed($log, activityFrames, { expandedKeys: activityExpandedKeys });
    if (stickToBottom) $log.scrollTop = $log.scrollHeight;
  }

  function rememberActivityExpansion() {
    const details = $detail.querySelectorAll("details[data-activity-key]");
    if (!details.length) return;
    activityExpandedKeys.clear();
    for (const item of details) {
      if (item.open && item.dataset.activityKey) activityExpandedKeys.add(item.dataset.activityKey);
    }
  }

  function openAddPanel() {
    $addPanel.hidden = false;
    $toggleAdd.setAttribute("aria-expanded", "true");
    requestAnimationFrame(() => (api.host.isDesktop ? $pick : $path).focus());
  }
  function closeAddPanel() {
    $addPanel.hidden = true;
    $toggleAdd.setAttribute("aria-expanded", "false");
    $path.classList.remove("is-drop-target");
  }

  function acceptDroppedPath(event) {
    event.preventDefault();
    $path.classList.remove("is-drop-target");
    const path = pathFromDrop(event);
    const decision = assessProjectPath({
      isDesktop: api.host.isDesktop,
      source: "drop",
      path,
      authorizedPath: authorizedDesktopPath,
    });
    if (decision.ok) {
      $path.value = decision.path;
      $path.focus();
    } else {
      api.toast(decision.message);
      if (api.host.isDesktop) $pick.focus();
    }
  }

  // ---- wiring ----
  $toggleAdd.addEventListener("click", () => {
    if ($addPanel.hidden) openAddPanel(); else closeAddPanel();
  });
  $cancelAdd.addEventListener("click", closeAddPanel);
  $add.addEventListener("click", add);
  $pick.addEventListener("click", pickFolder);
  $addTile.addEventListener("click", openAddPanel);
  $path.addEventListener("dragenter", (e) => {
    e.preventDefault();
    if (!api.host.isDesktop) $path.classList.add("is-drop-target");
  });
  $path.addEventListener("dragover", (e) => {
    e.preventDefault();
    if (e.dataTransfer) e.dataTransfer.dropEffect = api.host.isDesktop ? "none" : "copy";
  });
  $path.addEventListener("dragleave", () => {
    $path.classList.remove("is-drop-target");
  });
  $path.addEventListener("drop", acceptDroppedPath);
  for (const $i of [$path, $gameId, $groupId, $placeIds]) {
    $i.addEventListener("keydown", (e) => { if (e.key === "Enter") add(); });
  }
  $search.addEventListener("input", () => {
    searchQuery = $search.value;
    renderList();
  });
  $pills.addEventListener("click", (e) => {
    const btn = e.target.closest(".pill");
    if (!btn) return;
    const f = btn.dataset.filter;
    if (!f) return;
    filter = f;
    for (const b of $pills.querySelectorAll(".pill")) {
      b.setAttribute("aria-pressed", b.dataset.filter === f ? "true" : "false");
    }
    renderList();
  });
  $detail.addEventListener("click", async (e) => {
    const btn = e.target.closest("[data-copy-path]");
    if (!btn) return;
    const path = btn.dataset.copyPath || "";
    if (!path) return;
    try {
      await copyText(api, path);
      api.toast && api.toast("Path copied");
    } catch {
      api.toast && api.toast("Could not copy path");
    }
  });

  const offState = api.onBus("state", render);
  const offUp = api.onBus("daemon:up", refreshStatuses);
  const offDown = api.onBus("daemon:down", (event = {}) => {
    const projectId = event.projectId
      || (event.project && (api.getState().projects || []).find((p) => p.path === event.project)?.id)
      || api.getState().activeProjectId;
    if (projectId && event.error) startErrorByProject.set(projectId, String(event.error));
    refreshStatuses();
  });
  const offPluginShutdown = api.onBus("plugin:shutdown", (event = {}) => {
    handleRuntimeShutdown(event);
  });
  const offPluginState = api.onBus("plugin:state", (event = {}) => {
    handlePluginState(event);
  });
  const offProjectionRepair = api.onBus("projection:repair-state", (event = {}) => {
    if (!event.projectId || event.projectId === selectedId) render();
  });

  render();
  refreshStatuses();

  return () => {
    disposed = true;
    offState(); offUp(); offDown(); offPluginShutdown(); offPluginState(); offProjectionRepair();
    if (activityRaf) cancelAnimationFrame(activityRaf);
    if (skippedActivityTimer) clearTimeout(skippedActivityTimer);
    closeActivityStream();
  };
}

// ---- helpers ----
export function assessProjectPath({ isDesktop, source, path, authorizedPath = "" }) {
  const cleaned = trimProjectPath(path);
  if (isDesktop && source === "drop") {
    return {
      ok: false,
      message: "Use Browse… to authorize project folders in the desktop app",
    };
  }
  if (!cleaned) {
    return {
      ok: false,
      message: source === "drop" ? "Drop a folder path" : "Choose a project folder",
    };
  }
  if (isDesktop && cleaned !== trimProjectPath(authorizedPath)) {
    return {
      ok: false,
      message: "Use Browse… to authorize this project folder first",
    };
  }
  return { ok: true, path: cleaned };
}

function trimProjectPath(path) {
  const value = String(path ?? "").replace(/^﻿/, "").trim();
  if (/^\/+$/u.test(value)) return "/";
  if (/^[A-Za-z]:[\\/]+$/u.test(value)) return `${value.slice(0, 2)}\\`;
  return value.replace(/[\\/]+$/u, "");
}

function basename(p) {
  if (!p) return "";
  const s = p.replace(/[\\/]+$/, "");
  const i = Math.max(s.lastIndexOf("/"), s.lastIndexOf("\\"));
  return i >= 0 ? s.slice(i + 1) : s;
}
function leafInitials(name) {
  const s = String(name || "").trim();
  if (!s) return "·";
  const parts = s.split(/[\s_\-]+/).filter(Boolean);
  if (parts.length >= 2) return (parts[0][0] + parts[1][0]).toUpperCase();
  return s.slice(0, 2).toUpperCase();
}
function escapeHTML(s) {
  return String(s).replace(/[&<>"']/g, (c) => ({
    "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;",
  }[c]));
}

function projectFailureMarkup(
  failure,
  announce = true,
  repairState = null,
  daemonLocked = false,
) {
  const isMigration = failure.code === "legacy-init-leaf";
  const isRecovery = failure.code === "projection-recovery-required";
  const isRepairable = failure.code === "multiple-init-markers"
    || isMigration
    || isRecovery;
  const fileList = failure.files?.length
    ? `<div class="project-error-files" aria-label="${isMigration ? "Required filename migration" : "Conflicting files"}">
        ${failure.files.map((file) => `<code>${escapeHTML(file)}</code>`).join(`<span aria-hidden="true">${isMigration ? "→" : "or"}</span>`)}
      </div>`
    : "";
  const openLabel = failure.code === "multiple-init-markers" || isMigration
    ? "Open conflict folder"
    : "Open project folder";
  const repairLabel = isMigration
    ? "Fix filename offline"
    : isRecovery
      ? "Review recovery"
      : "Compare & resolve";
  const repairBusy = repairState?.status === "loading" || repairState?.status === "resolving";
  const repairLocksDaemon = daemonLocked
    || repairState?.status === "resolving"
    || repairState?.status === "recovery";
  const repairButton = isRepairable
    ? `<button type="button" class="primary" data-error-act="repair" ${repairBusy ? 'disabled aria-busy="true"' : ""}>${
        repairState?.status === "loading"
          ? "Inspecting…"
          : repairState?.status === "resolving"
            ? "Resolving…"
            : repairLabel
      }</button>`
    : "";
  return `
    <section class="project-error" role="${announce ? "alert" : "region"}" ${announce ? 'aria-live="assertive"' : 'aria-label="Project startup error"'} data-error-code="${escapeHTML(failure.code)}">
      <div class="project-error-main">
        <span class="project-error-icon" aria-hidden="true">${alertSVG()}</span>
        <div class="project-error-copy">
          <h2>${escapeHTML(failure.title)}</h2>
          <p>${escapeHTML(failure.summary)}</p>
        </div>
      </div>
      <p class="project-error-guidance">${escapeHTML(failure.guidance)}</p>
      ${fileList}
      <div class="project-error-actions">
        ${repairButton}
        <button type="button" class="${isRepairable ? "" : "primary"}" data-error-act="retry" ${repairLocksDaemon ? `disabled${repairState?.status === "resolving" ? ' aria-busy="true"' : ""}` : ""}>Retry daemon</button>
        <button type="button" data-error-act="open">${openLabel}</button>
        <button type="button" data-error-act="copy">Copy diagnostic</button>
      </div>
      <details class="project-error-details">
        <summary data-error-act="details">
          <span>Technical details</span>
          <span class="project-error-chevron" aria-hidden="true">${chevronDownSVG()}</span>
        </summary>
        <pre><code>${escapeHTML(failure.diagnostic)}</code></pre>
      </details>
    </section>
  `;
}

function normalizeRepairPath(path) {
  return String(path || "")
    .replace(/\\/g, "/")
    .replace(/\/+/g, "/")
    .replace(/\/+$/, "");
}

function chooseProjectionConflict(report, failure, preferredId = "") {
  const conflicts = report?.conflicts || [];
  if (preferredId) {
    const preferred = conflicts.find((conflict) => conflict.id === preferredId);
    if (preferred) return preferred;
  }
  const expectedKind = failure?.code === "legacy-init-leaf"
    ? "legacy-reserved-init-leaf"
    : failure?.code;
  const failurePath = normalizeRepairPath(failure?.path);
  return conflicts.find((conflict) => {
    if (expectedKind && conflict.kind !== expectedKind) return false;
    const directory = normalizeRepairPath(conflict.directory);
    return !failurePath || !directory
      || failurePath === directory
      || failurePath.endsWith(`/${directory}`);
  }) || conflicts.find((conflict) => conflict.kind === expectedKind) || conflicts[0] || null;
}

function currentProjectionConflict(state, failure) {
  return chooseProjectionConflict(state?.report, failure, state?.currentId);
}

function projectionDiffPane(rows, side, file) {
  if (!file?.utf8) {
    return `<div class="projection-code-empty">Preview unavailable — this source is not valid UTF-8.</div>`;
  }
  return `
    <div class="projection-code" role="region" aria-label="${escapeHTML(file.name)} source preview" tabindex="0">
      ${rows.map((row) => {
        const line = row[side];
        if (!line) {
          return `<div class="projection-code-line is-blank" aria-hidden="true"><span></span><code> </code></div>`;
        }
        return `<div class="projection-code-line is-${line.kind}">
          <span aria-hidden="true">${line.number}</span>
          <code>${escapeHTML(line.text || " ")}</code>
        </div>`;
      }).join("")}
    </div>
  `;
}

function projectionSinglePreview(file) {
  const lines = String(file?.preview || "").split(/\r?\n/).slice(0, 240);
  const rows = lines.map((line, index) => ({
    left: { number: index + 1, text: line, kind: "same" },
  }));
  return projectionDiffPane(rows, "left", file);
}

function projectionFileCard(file, index, state, diffRows = null, side = "left", choose = true) {
  const selected = state.selectedKeep === file.name;
  const preview = diffRows
    ? projectionDiffPane(diffRows, side, file)
    : projectionSinglePreview(file);
  return `
    <article class="projection-file ${selected ? "is-selected" : ""}">
      <header class="projection-file-head">
        <div class="projection-file-title">
          <code>${escapeHTML(file.name)}</code>
          <span>${escapeHTML(markerStyleLabel(file.style))}</span>
        </div>
        <div class="projection-file-meta">
          <span>${escapeHTML(file.className)}</span>
          <span>${escapeHTML(formatFileBytes(file.size))}</span>
          <span title="${escapeHTML(file.sha256)}">sha ${escapeHTML(shortHash(file.sha256))}</span>
        </div>
      </header>
      ${preview}
      ${file.previewTruncated ? `<div class="projection-preview-note">Preview truncated · full file protected by SHA-256</div>` : ""}
      ${choose ? `<button type="button" class="${selected ? "primary" : ""}" data-repair-choose="${index}" aria-pressed="${selected}" ${state.status === "resolving" ? "disabled" : ""}>
        ${selected ? checkSVG() : ""}<span>${selected ? "Selected" : `Keep ${escapeHTML(file.name)}`}</span>
      </button>` : ""}
    </article>
  `;
}

function projectionRepairMarkup(state, failure) {
  if (state.status === "loading") {
    return `
      <section class="projection-repair is-loading" aria-live="polite" aria-busy="true">
        <div class="projection-repair-kicker"><span class="dot dot-idle"></span>Offline repair</div>
        <h2>Inspecting the disk projection…</h2>
        <p>The daemon and Roblox Studio can stay closed while Ro Sync reads these files.</p>
        <div class="projection-loading-bar" aria-hidden="true"></div>
      </section>
    `;
  }

  if (state.status === "error") {
    return `
      <section class="projection-repair is-error" role="alert">
        <div class="projection-repair-kicker"><span class="dot dot-err"></span>Offline repair</div>
        <h2>Ro Sync could not inspect this conflict</h2>
        <p>${escapeHTML(state.error || "The projection inspection failed.")}</p>
        <div class="projection-repair-actions">
          <button type="button" class="primary" data-repair-act="refresh">Try inspection again</button>
          <button type="button" data-repair-act="close">Close</button>
        </div>
      </section>
    `;
  }

  if (state.status === "recovery") {
    const resolution = state.report?.resolution || {};
    const receiptAvailable = resolution.receiptAvailable === true;
    const receiptPath = resolution.receiptPath || "";
    const recoveryId = resolution.id || "";
    const recoveryActions = new Set(resolution.recoveryActions || []);
    const canResume = !!recoveryId && recoveryActions.has("resume");
    const canQuarantine = !!recoveryId && recoveryActions.has("quarantine");
    const recoveryGuidance = canResume
      ? "Resume replays the already-recorded decision, verifies every source and destination hash, and refuses ambiguous state."
      : canQuarantine
        ? "The transaction cannot be replayed. After reviewing the folder, you may quarantine only its broken recovery record; Ro Sync will still require a complete clean projection scan."
        : "This recovery record has no safe automated action. Review the backup and conflicting files before trying a newer Ro Sync build or the CLI.";
    return `
      <section class="projection-repair is-error projection-repair-recovery" role="alert">
        <div class="projection-repair-kicker"><span class="dot dot-err"></span>Recovery required</div>
        <h2>The repair stopped before Ro Sync could prove the final state</h2>
        <p>${escapeHTML(state.error || (
          receiptAvailable
            ? "Ro Sync preserved a recovery receipt for the uncertain transaction."
            : "Ro Sync could not prove that its recovery receipt is still reachable from this project path."
        ))}</p>
        <div class="projection-recovery-receipt">
          <span>Recovery receipt</span>
          <code>${receiptAvailable && receiptPath
            ? escapeHTML(receiptPath)
            : "Unavailable — inspect the backup root and conflicting folder manually"}</code>
        </div>
        <p>Ro Sync will not retry the daemon from this state. ${escapeHTML(recoveryGuidance)}</p>
        ${state.confirmQuarantine ? `
          <div class="projection-confirm is-danger" role="group" aria-label="Confirm recovery record quarantine">
            <div>
              <strong>Quarantine only this recovery record?</strong>
              <span>This does not restore or delete source files. Ro Sync will move the transaction record aside, rescan the whole projection, and keep the daemon stopped unless that scan is proven clean.</span>
            </div>
            <div class="projection-confirm-actions">
              <button type="button" data-repair-act="cancel-quarantine">Cancel</button>
              <button type="button" class="danger" data-repair-act="confirm-quarantine">Quarantine &amp; rescan</button>
            </div>
          </div>
        ` : ""}
        <div class="projection-repair-actions">
          ${canResume ? `<button type="button" class="primary" data-repair-act="resume-recovery">Resume verified repair</button>` : ""}
          <button type="button" class="${canResume ? "" : "primary"}" data-repair-act="open-backup">Open recovery folder</button>
          ${canQuarantine && !state.confirmQuarantine ? `<button type="button" class="danger" data-repair-act="quarantine-recovery">Quarantine record…</button>` : ""}
          <button type="button" data-repair-act="refresh">Refresh recovery</button>
          <button type="button" data-repair-act="close">Close</button>
        </div>
      </section>
    `;
  }

  const report = state.report;
  const conflict = currentProjectionConflict(state, failure);
  if (!conflict) {
    if (report?.truncated || report?.remaining > 0) {
      return `
        <section class="projection-repair is-error" role="alert">
          <div class="projection-repair-kicker"><span class="dot dot-warn"></span>Offline repair</div>
          <h2>${escapeHTML(String(report.remaining || report.totalConflicts || "More"))} startup blockers could not be displayed</h2>
          <p>Ro Sync kept the daemon stopped because this report is incomplete. Refresh after updating Ro Sync or review the project from the CLI.</p>
          <div class="projection-repair-actions">
            <button type="button" class="primary" data-repair-act="refresh">Refresh inspection</button>
            <button type="button" data-repair-act="close">Close</button>
          </div>
        </section>
      `;
    }
    return `
      <section class="projection-repair" aria-live="polite">
        <div class="projection-repair-kicker"><span class="dot dot-ok"></span>Offline repair</div>
        <h2>No projection blockers were found</h2>
        <p>The files may have changed since the startup error. Retry the daemon, or refresh this check.</p>
        <div class="projection-repair-actions">
          <button type="button" data-repair-act="refresh">Refresh inspection</button>
          <button type="button" data-repair-act="close">Close</button>
        </div>
      </section>
    `;
  }

  const isLegacy = conflict.kind === "legacy-reserved-init-leaf";
  const files = conflict.files || [];
  const currentIndex = Math.max(0, report.conflicts.findIndex((item) => item.id === conflict.id));
  const classesDiffer = new Set(files.map((file) => file.className)).size > 1;
  const filesTruncated = !!conflict.filesTruncated;
  const blockerTotal = report.totalConflicts || report.conflicts.length;
  const liveStatus = state.notice
    || (state.selectedKeep
      ? `${state.selectedKeep} selected. Confirmation is available.`
      : `Viewing startup blocker ${currentIndex + 1} of ${blockerTotal}.`);
  const relation = conflict.identical && !classesDiffer
    ? `<div class="projection-relation is-identical">${checkSVG()}<span>The source bytes are identical. Choosing a layout will not change the code.</span></div>`
    : conflict.identical
      ? `<div class="projection-relation is-warning">${alertSVG()}<span>The source bytes match, but the script classes differ. Choose the intended class.</span></div>`
      : !isLegacy
        ? `<div class="projection-relation is-warning">${alertSVG()}<span>These sources differ. Ro Sync will not guess which one you intended.</span></div>`
        : "";
  const completenessWarning = filesTruncated
    ? `<div class="projection-repair-error" role="alert">This conflict has more marker files than the renderer can safely display. Refresh after updating Ro Sync; resolution is disabled.</div>`
    : "";
  const reportPageWarning = report.truncated
    ? `<div class="projection-relation is-warning">${alertSVG()}<span>Showing ${report.conflicts.length} of ${blockerTotal} blockers. Ro Sync will reveal the next page after a visible blocker is resolved.</span></div>`
    : "";

  let body = "";
  if (isLegacy) {
    const sourceName = basename(conflict.sourcePath || files[0]?.name || "");
    const targetName = basename(conflict.canonicalPath || "");
    body = `
      <div class="projection-rename">
        <code>${escapeHTML(sourceName)}</code>
        <span aria-hidden="true">→</span>
        <code>${escapeHTML(targetName)}</code>
      </div>
      ${files[0] ? projectionFileCard(files[0], 0, state, null, "left", false) : ""}
      <div class="projection-confirm">
        <div>
          <strong>Preserve the script; fix only its disk spelling</strong>
          <span>The escaped filename still maps to the same Roblox script name.</span>
        </div>
        <button type="button" class="primary" data-repair-act="migrate" ${state.status === "resolving" ? 'disabled aria-busy="true"' : ""}>
          ${state.status === "resolving" ? "Renaming…" : "Rename safely"}
        </button>
      </div>
    `;
  } else {
    const firstTwo = files.slice(0, 2);
    const diff = firstTwo.length === 2
      ? buildProjectionLineDiff(firstTwo[0].preview, firstTwo[1].preview)
      : null;
    const cards = files.map((file, index) => projectionFileCard(
      file,
      index,
      state,
      index < 2 ? diff?.rows : null,
      index === 1 ? "right" : "left",
      !filesTruncated,
    )).join("");
    const otherNames = files
      .filter((file) => file.name !== state.selectedKeep)
      .map((file) => file.name);
    body = `
      ${relation}
      ${completenessWarning}
      <div class="projection-files-grid">${cards}</div>
      ${diff?.approximate ? `<div class="projection-preview-note">Large preview: showing a bounded positional comparison.</div>` : ""}
      ${diff?.truncated ? `<div class="projection-preview-note">Only the first 240 preview lines are rendered.</div>` : ""}
      ${state.selectedKeep && !filesTruncated ? `
        <div class="projection-confirm" role="group" aria-label="Confirm conflict resolution">
          <div>
            <strong>Keep ${escapeHTML(state.selectedKeep)}</strong>
            <span>${otherNames.length === 1
              ? `${escapeHTML(otherNames[0])} will be moved to .rosync-backups.`
              : `${otherNames.length} other markers will be moved to .rosync-backups.`} Nothing is deleted.</span>
          </div>
          <div class="projection-confirm-actions">
            <button type="button" data-repair-act="cancel-choice" ${state.status === "resolving" ? "disabled" : ""}>Cancel</button>
            <button type="button" class="primary" data-repair-act="confirm" ${state.status === "resolving" ? 'disabled aria-busy="true"' : ""}>
              ${state.status === "resolving" ? "Archiving…" : "Archive other & continue"}
            </button>
          </div>
        </div>
      ` : ""}
    `;
  }

  return `
    <section class="projection-repair">
      <span class="sr-only" role="status" aria-live="polite" aria-atomic="true">${escapeHTML(liveStatus)}</span>
      <header class="projection-repair-head">
        <div>
          <div class="projection-repair-kicker"><span class="dot dot-warn"></span>Offline repair · blocker ${currentIndex + 1} of ${blockerTotal}</div>
          <h2>${isLegacy
            ? "Escape this literal script filename"
            : `Choose the source for ${escapeHTML(basename(conflict.directory))}`}</h2>
          <p>${isLegacy
            ? "This is a deterministic filename migration; the source and Studio name stay unchanged."
            : "Both choices are local disk files. Choose the one that should define the parent script."}</p>
        </div>
        <button type="button" class="projection-close" data-repair-act="close" aria-label="Close offline repair" ${state.status === "resolving" ? "disabled" : ""}>${xSVG()}</button>
      </header>
      <div class="projection-path" title="${escapeHTML(conflict.directory)}">${escapeHTML(conflict.directory)}</div>
      ${state.notice ? `<div class="projection-repair-notice">${checkSVG()}<span>${escapeHTML(state.notice)}</span>${
        state.report?.resolution?.backupPaths?.length
          ? `<button type="button" data-repair-act="open-backup">Open backup</button>`
          : ""
      }</div>` : ""}
      ${state.error ? `<div class="projection-repair-error" role="alert">${escapeHTML(state.error)}</div>` : ""}
      ${reportPageWarning}
      ${body}
      ${report.conflicts.length > 1 ? `
        <div class="projection-stepper" aria-label="Startup blockers">
          ${report.conflicts.map((item, index) => `
            <button type="button" data-repair-conflict="${index}" aria-current="${item.id === conflict.id ? "step" : "false"}" title="${escapeHTML(item.directory)}" ${state.status === "resolving" ? "disabled" : ""}>
              ${index + 1}
            </button>
          `).join("")}
        </div>
      ` : ""}
      <footer class="projection-repair-foot">
        <span>Studio truth is still available after startup through initial sync.</span>
        <button type="button" data-repair-act="refresh" ${state.status === "resolving" ? "disabled" : ""}>Refresh files</button>
      </footer>
    </section>
  `;
}

async function writeTextFile(api, absPath, text) {
  return api.host.writeTextFile(absPath, text);
}
async function readTextFile(api, absPath) {
  return api.host.readTextFile(absPath);
}
function normalizeStudioPathForDisplay(path) {
  return String(path || "")
    .trim()
    .replace(/\\/g, "/")
    .replace(/^\/+|\/+$/g, "")
    .replace(/\/{2,}/g, "/");
}

/**
 * Validate the Studio-style folder used to locate wally.toml.
 *
 * This is deliberately stricter than display normalization: every component
 * must survive exactly as entered so `..`, `.`, doubled separators, and
 * control characters can never be turned into a path outside the project.
 */
export function validateWallyFolder(path) {
  const raw = String(path ?? "");
  if (/[\u0000-\u001f\u007f-\u009f]/u.test(raw)) {
    throw new Error("Wally folder cannot contain control characters");
  }

  const value = raw.trim().replace(/\\/g, "/");
  if (!value) throw new Error("Wally folder is required");
  if (value.startsWith("/") || /^[A-Za-z]:(?:\/|$)/.test(value)) {
    throw new Error("Wally folder must be relative to the project");
  }

  const components = value.split("/");
  if (components.some((component) => !component || !component.trim())) {
    throw new Error("Wally folder cannot contain empty path components");
  }
  if (components.some((component) => component === "." || component === "..")) {
    throw new Error("Wally folder cannot contain . or .. path components");
  }
  return components.join("/");
}

export function wallyTomlPathForFolder(projectPath, studioFolder) {
  const normalized = validateWallyFolder(studioFolder);
  const parts = normalized.split("/");
  const parent = parts.length > 1 ? parts.slice(0, -1).join("/") : "";
  return joinProjectFile(projectPath, parent ? `${parent}/wally.toml` : "wally.toml");
}
function wallyTomlPathPreview(projectPath, studioFolder) {
  try {
    return `Writes to ${wallyTomlPathForFolder(projectPath, studioFolder)}`;
  } catch (error) {
    return `Invalid Wally folder: ${error.message}`;
  }
}
function dirnamePath(path) {
  const s = String(path || "").replace(/[\\/]+$/, "");
  const i = Math.max(s.lastIndexOf("/"), s.lastIndexOf("\\"));
  return i >= 0 ? s.slice(0, i) : ".";
}
function summarizeWallyInstall(res) {
  const out = `${res?.stdout || ""}\n${res?.stderr || ""}`.trim();
  const installed = out.match(/Installed\s+(\d+)\s+packages?/i);
  const downloaded = out.match(/Downloaded\s+(\d+)\s+packages?/i);
  if (installed && downloaded) return `Wally installed ${installed[1]} packages (${downloaded[1]} downloaded)`;
  if (installed) return `Wally installed ${installed[1]} packages`;
  if (downloaded) return `Wally install complete (${downloaded[1]} downloaded)`;
  return "Wally install complete";
}
function shortStudioPath(path) {
  const normalized = normalizeStudioPathForDisplay(path);
  if (!normalized) return "enabled";
  const parts = normalized.split("/");
  if (parts.length <= 2) return normalized;
  return `${parts[0]}/…/${parts[parts.length - 1]}`;
}
function formatRelative(ts) {
  if (!ts) return "";
  const t = typeof ts === "number" ? ts : Date.parse(ts);
  if (!Number.isFinite(t)) return "";
  const diff = Math.max(0, Date.now() - t);
  const sec = Math.floor(diff / 1000);
  if (sec < 5) return "just now";
  if (sec < 60) return `${sec}s ago`;
  const min = Math.floor(sec / 60);
  if (min < 60) return `${min}m ago`;
  const hr = Math.floor(min / 60);
  if (hr < 24) return `${hr}h ago`;
  const d = Math.floor(hr / 24);
  return `${d}d ago`;
}
function pluginStatusLabel(isActive, daemonOk, st) {
  if (!isActive) return "Inactive · serve this project to connect";
  if (!daemonOk) return "Daemon offline";
  if (st.kind === "ok") return "Daemon reachable";
  return "Waiting for daemon…";
}
function plusSVG() {
  return '<svg viewBox="0 0 16 16" width="14" height="14" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" aria-hidden="true">' +
    '<path d="M8 3.5v9M3.5 8h9"/>' +
    '</svg>';
}
function chevronLeftSVG() {
  return '<svg viewBox="0 0 16 16" width="14" height="14" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">' +
    '<path d="M10 3.5 5.5 8 10 12.5"/>' +
    '</svg>';
}
function editSVG() {
  return '<svg viewBox="0 0 16 16" width="13" height="13" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">' +
    '<path d="M9.75 3.25 12.75 6.25"/>' +
    '<path d="M2.75 10.75 10.9 2.6a1.25 1.25 0 0 1 1.77 0l.73.73a1.25 1.25 0 0 1 0 1.77l-8.15 8.15-3 .75.5-3.25z"/>' +
    '</svg>';
}
function sessionSVG() {
  return '<svg viewBox="0 0 16 16" width="13" height="13" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">' +
    '<path d="M2.25 3.5a1.25 1.25 0 0 1 1.25-1.25h9a1.25 1.25 0 0 1 1.25 1.25v5.75a1.25 1.25 0 0 1-1.25 1.25H7l-3.25 3v-3H3.5a1.25 1.25 0 0 1-1.25-1.25V3.5z"/>' +
    '<path d="M5.25 5.5h5.5M5.25 7.75h3.5"/>' +
    '</svg>';
}
function xSVG() {
  return '<svg viewBox="0 0 16 16" width="13" height="13" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" aria-hidden="true">' +
    '<path d="m4.25 4.25 7.5 7.5M11.75 4.25l-7.5 7.5"/>' +
    '</svg>';
}
function checkSVG() {
  return '<svg viewBox="0 0 16 16" width="14" height="14" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">' +
    '<path d="m3.25 8.25 3 3 6.5-6.5"/>' +
    '</svg>';
}
function folderSVG() {
  return '<svg viewBox="0 0 16 16" width="18" height="18" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linejoin="round" aria-hidden="true">' +
    '<path d="M1.75 4.25a1 1 0 0 1 1-1h3.5l1.5 1.5h6.5a1 1 0 0 1 1 1v6.5a1 1 0 0 1-1 1H2.75a1 1 0 0 1-1-1v-7z"/>' +
    '</svg>';
}
function alertSVG() {
  return '<svg viewBox="0 0 16 16" width="16" height="16" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" aria-hidden="true">' +
    '<circle cx="8" cy="8" r="6.25"/>' +
    '<path d="M8 4.5v4.25M8 11.25v.25"/>' +
    '</svg>';
}
function chevronDownSVG() {
  return '<svg viewBox="0 0 16 16" width="14" height="14" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">' +
    '<path d="m4 6 4 4 4-4"/>' +
    '</svg>';
}
