// views/projects.js — list+detail Projects view with sidebar-friendly shell,
// per-project controls, and a throttled activity tail for the served project.
import { daemonJson, daemonWS } from "../bridge.js";
import { copyText, pathFromDrop } from "./runtime.js";
import {
  pickFolderCmd,
  openFolderEnsuredCmd,
  writeFileFromB64Cmd,
  readFileCmd,
  shQuote,
  psEncodedCmd,
  psQuote,
  IS_WINDOWS,
} from "../platform.js";

const MAX_PROJECT_LOG_LINES = 100;
const MAX_PROJECT_PARSED_OPS_PER_SECOND = 20;
const RAW_OP_RE = /"type"\s*:\s*"op"/;
const DEFAULT_WALLY_FOLDER = "ReplicatedStorage/Assets/Packages";
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
        <aside class="ws-detail" id="proj-detail" aria-live="polite"></aside>
      </div>
    </div>
  `;

  const $addPanel = root.querySelector("#proj-add-panel");
  const $toggleAdd = root.querySelector("#proj-toggle-add");
  const $cancelAdd = root.querySelector("#proj-cancel-add");
  const $path = root.querySelector("#proj-path");
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
  let activityWs = null;
  let activityRaf = 0;
  let activityProjectId = initialState.activeProjectId || null;
  let activityRawWindowStart = 0;
  let activityParsedOpsInWindow = 0;
  let skippedActivityOps = 0;
  let skippedActivityTimer = 0;
  let disposed = false;
  const activityFrames = [];

  function parsePlaceIds(raw) {
    if (!raw) return [];
    return String(raw).split(",").map((s) => s.trim()).filter(Boolean);
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
        if (p.id !== s.activeProjectId) return false;
        const st = snapshotByProject.get(p.id);
        if (!st || st.ok !== true) return false;
      } else if (filter === "needs-setup") {
        if (p.gameId || p.groupId) return false;
      }
      return true;
    });
  }

  function statusFor(p) {
    const s = api.getState();
    const st = snapshotByProject.get(p.id) || {};
    if (p.id !== s.activeProjectId) return { kind: "idle", label: "Not Serving", dot: "dot-idle" };
    if (st.ok === true) return { kind: "ok", label: "Serving", dot: "dot-ok" };
    if (st.ok === false) return { kind: "err", label: st.label || "Error", dot: "dot-err" };
    return { kind: "idle", label: st.label || "Starting…", dot: "dot-idle" };
  }

  function renderList() {
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
      const isServing = p.id === s.activeProjectId;
      const st = statusFor(p);
      const dupeGroups = (snapshotByProject.get(p.id) || {}).dupeGroups || 0;

      li.innerHTML = `
        <span class="thumb">${escapeHTML(initials)}</span>
        <div class="meta">
          <span class="name"></span>
          <span class="path"></span>
        </div>
        <label class="switch toggle" title="${isServing ? "Stop serving" : "Start serving"}" data-act="serve-wrap">
          <input type="checkbox" data-act="serve" ${isServing ? "checked" : ""} aria-label="Serve this project" />
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
    const s = api.getState();
    const projects = s.projects || [];
    const p = projects.find((x) => x.id === selectedId)
      || projects.find((x) => x.id === s.activeProjectId)
      || projects[0]
      || null;

    if (!p) {
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
      renderDetailEdit(p);
      return;
    }

    const st = statusFor(p);
    const initials = leafInitials(p.name || basename(p.path));
    const lastSync = (snapshotByProject.get(p.id) || {}).lastSync || null;
    const lastSyncLabel = formatRelative(lastSync);
    const isActive = p.id === s.activeProjectId;

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
        <button class="detail-icon-btn" data-act="spawn-session" type="button" title="Spawn Session" aria-label="Spawn Session">${sessionSVG()}<span>Spawn Session</span></button>
        <button class="detail-icon-btn" data-act="edit" type="button" title="Edit project" aria-label="Edit project">${editSVG()}<span>Edit</span></button>
      </div>
      <div class="project-log-shell">
        <div class="project-log-head">
          <span>Recent actions</span>
          <span class="muted-sm">${isActive ? "last 100" : "inactive"}</span>
        </div>
        <div class="project-log" data-project-log aria-live="polite"></div>
      </div>
    `;

    $detail.querySelector(".name").textContent = p.name || basename(p.path);

    $detail.querySelector('[data-act="back"]').addEventListener("click", () => {
      $workspace.dataset.mode = "list";
    });
    $detail.querySelector('[data-act="spawn-session"]').addEventListener("click", () => {
      spawnSession(p);
    });
    $detail.querySelector('[data-act="edit"]').addEventListener("click", () => {
      beginEdit(p);
    });
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
    const isActive = p.id === s.activeProjectId;
    const daemonOk = !!api.getDaemonBase();
    const wallyEnabled = !!p.wallyEnabled;
    const wallyFolder = p.wallyFolder || (wallyEnabled ? DEFAULT_WALLY_FOLDER : "");
    const wallyFile = p.wallyFile || DEFAULT_WALLY_FILE;
    const wallyTomlPath = wallyTomlPathForFolder(p.path, wallyFolder || DEFAULT_WALLY_FOLDER);

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
                <div class="project-hint" data-wally-target>Writes to ${escapeHTML(wallyTomlPath)}</div>
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
      const folder = normalizeStudioPath($wallyFolder.value || DEFAULT_WALLY_FOLDER);
      $wallyTarget.textContent = `Writes to ${wallyTomlPathForFolder(p.path, folder)}`;
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
        wallyFolder: normalizeStudioPath($wallyFolder.value),
        wallyFile: $wallyFile.value,
      });
    });
    $wallyInstall.addEventListener("click", () => {
      installWallyFromEdit(p, {
        enabled: $wallyEnabled.checked,
        folder: normalizeStudioPath($wallyFolder.value),
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
    renderList();
    renderDetail();
    api.setStatus(`${(api.getState().projects || []).length} project(s)`);
    ensureActivityStream();
  }

  function selectProject(id) {
    selectedId = id;
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
    const nextWallyFolder = normalizeStudioPath(form.wallyFolder);
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

    if (id === s.activeProjectId && changedLaunchArgs) {
      if (typeof api.killDaemon === "function") {
        try { await api.killDaemon(); } catch (e) { console.warn("killDaemon", e); }
      }
      if (typeof api.ensureDaemon === "function") {
        try { await api.ensureDaemon(); } catch (e) { console.warn("ensureDaemon", e); }
      }
      api.toast("Saved — daemon restarted");
    } else {
      api.toast("Saved");
    }
  }

  async function add() {
    const path = $path.value.trim();
    if (!path) return;
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
    $gameId.value = "";
    $groupId.value = "";
    $placeIds.value = "";
    closeAddPanel();
    render();
    refreshStatuses();
  }

  async function remove(id) {
    const s = api.getState();
    const wasServing = s.activeProjectId === id;
    const next = (s.projects || []).filter((p) => p.id !== id);
    api.setState({
      projects: next,
      activeProjectId: wasServing ? null : s.activeProjectId,
    });
    if (wasServing && typeof api.killDaemon === "function") {
      try { await api.killDaemon(); } catch (e) { console.warn("killDaemon", e); }
    }
    snapshotByProject.delete(id);
    if (editingId === id) editingId = null;
    if (selectedId === id) selectedId = (next[0] && next[0].id) || null;
    $workspace.dataset.mode = "list";
    render();
  }

  function serve(id) {
    api.setState({ activeProjectId: id });
    selectedId = id;
    render();
    refreshStatuses();
  }

  async function stopServing(id) {
    const s = api.getState();
    if (s.activeProjectId !== id) { render(); return; }
    api.setState({ activeProjectId: null });
    if (typeof api.killDaemon === "function") {
      try { await api.killDaemon(); } catch (e) { console.warn("killDaemon", e); }
    }
    render();
    refreshStatuses();
  }

  async function pickFolder() {
    try {
      const res = await api.t64("t64:exec", {
        command: pickFolderCmd("Pick Ro Sync project folder"),
      });
      const raw = (res?.stdout || "").replace(/^﻿/, "").trim();
      const out = raw.replace(/[\\/]+$/, "");
      const looksLikePath =
        out && !/\r?\n/.test(out) && /^(?:[A-Za-z]:[\\/]|[\\/]|~)/.test(out);
      if (looksLikePath) {
        $path.value = out;
      } else if (!out) {
        // user cancelled — silent
      } else {
        api.toast("Folder picker failed");
        console.warn("pickFolder: ignoring non-path stdout", { raw, stderr: res?.stderr });
      }
    } catch (e) {
      api.toast("Folder picker unavailable");
    }
  }

  async function openFolder(p) {
    try {
      await api.t64("t64:exec", { command: openFolderEnsuredCmd(p) });
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
    const wallyFolder = normalizeStudioPath(cfg.wallyFolder ?? proj.wallyFolder ?? "");
    let wallyFile = typeof cfg.wallyFile === "string" ? cfg.wallyFile : (proj.wallyFile || "");
    if (wallyEnabled && !wallyFile) {
      try {
        wallyFile = await readTextFile(api, wallyTomlPathForFolder(proj.path, wallyFolder || DEFAULT_WALLY_FOLDER));
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
      merged.wallyFolder = normalizeStudioPath(proj.wallyFolder || DEFAULT_WALLY_FOLDER);
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

    const folder = normalizeStudioPath(form.folder);
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
    const wasActive = s.activeProjectId === proj.id;

    try {
      const nextProj = { ...proj, wallyEnabled: true, wallyFolder: folder, wallyFile: file };
      api.setState({
        projects: (s.projects || []).map((p) => p.id === proj.id ? nextProj : p),
      });
      await saveProjectConfig(nextProj);
      if (statusEl) statusEl.textContent = "Running wally install...";
      const res = await api.t64("t64:exec", {
        command: wallyInstallCmd(cwd),
        timeoutMs: 2 * 60_000,
      });
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
    const base = api.getDaemonBase();
    const s = api.getState();
    if (id !== s.activeProjectId || !base) {
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
      const bounds = await api.t64("t64:get-bounds", { timeoutMs: 1000 });
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
      const res = await api.t64("t64:create-session", { ...payload, timeoutMs: 10000 });
      if (res && res.error) throw new Error(res.error);
      api.toast("Session spawned");
    } catch (e) {
      console.warn("spawn session failed", e);
      api.toast("Spawn session failed");
    }
  }

  async function refreshStatuses() {
    const base = api.getDaemonBase();
    const s = api.getState();
    for (const p of s.projects || []) {
      if (p.id !== s.activeProjectId) {
        snapshotByProject.set(p.id, { ok: null, label: "inactive" });
        continue;
      }
      if (!base) {
        snapshotByProject.set(p.id, { ok: null, label: "daemon offline" });
        continue;
      }
      try {
        const info = await daemonJson(base, "/snapshot");
        snapshotByProject.set(p.id, {
          ok: true,
          label: info.summary || "synced",
          lastSync: info.lastSync,
          dupeGroups: countDupeGroups(info),
        });
      } catch (e) {
        snapshotByProject.set(p.id, { ok: false, label: e.message });
      }
    }
    render();
  }

  function ensureActivityStream() {
    const s = api.getState();
    const nextProjectId = s.activeProjectId || null;
    if (nextProjectId !== activityProjectId) {
      closeActivityStream();
      activityProjectId = nextProjectId;
      activityFrames.length = 0;
      activityRawWindowStart = 0;
      activityParsedOpsInWindow = 0;
      skippedActivityOps = 0;
      scheduleActivityRender();
    }

    const base = api.getDaemonBase();
    if (!base || !activityProjectId) {
      closeActivityStream();
      return;
    }
    if (activityWs) return;

    try {
      activityWs = daemonWS(base, "/ws", {
        skipRaw: shouldSkipRawActivityFrame,
        error: () => {
          pushActivityFrame({ type: "error", message: "activity stream error" });
        },
        close: () => {},
        message: (data) => {
          if (!data || typeof data !== "object") return;
          const t = data.type;
          if (t === "ping" || t === "pong" || t === "hello" || t === "lagged"
              || t === "push-result" || t === "error") return;
          if (t === "plugin") {
            pushActivityFrame({
              type: "plugin",
              message: data.connected ? "plugin connected" : "plugin disconnected",
            });
            return;
          }
          pushActivityFrame(data);
        },
      });
    } catch (e) {
      pushActivityFrame({ type: "error", message: `activity stream failed: ${e.message}` });
    }
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
      message: `collapsed ${count} daemon events while the project log was saturated`,
    });
  }

  function closeActivityStream() {
    if (!activityWs) return;
    const ws = activityWs;
    activityWs = null;
    try { ws.close(); } catch {}
  }

  function pushActivityFrame(frame) {
    if (disposed) return;
    activityFrames.push({ at: Date.now(), frame });
    while (activityFrames.length > MAX_PROJECT_LOG_LINES) activityFrames.shift();
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
    const s = api.getState();
    const selectedIsActive = selectedId && selectedId === s.activeProjectId;
    if (!selectedIsActive) {
      $log.innerHTML = `<span class="project-log-empty">Start serving this project to see actions.</span>`;
      return;
    }
    if (!activityFrames.length) {
      $log.innerHTML = `<span class="project-log-empty">Waiting for project actions…</span>`;
      return;
    }

    const stickToBottom = $log.scrollHeight - $log.scrollTop - $log.clientHeight < 32;
    const fragment = document.createDocumentFragment();
    for (const entry of activityFrames) {
      const line = renderActivityLine(entry);
      if (line) fragment.appendChild(line);
    }
    $log.innerHTML = "";
    $log.appendChild(fragment);
    if (stickToBottom) $log.scrollTop = $log.scrollHeight;
  }

  function openAddPanel() {
    $addPanel.hidden = false;
    $toggleAdd.setAttribute("aria-expanded", "true");
    requestAnimationFrame(() => $path.focus());
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
    if (path) {
      $path.value = path;
      $path.focus();
    } else {
      api.toast("Drop a folder path");
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
    $path.classList.add("is-drop-target");
  });
  $path.addEventListener("dragover", (e) => {
    e.preventDefault();
    if (e.dataTransfer) e.dataTransfer.dropEffect = "copy";
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
  const offDown = api.onBus("daemon:down", refreshStatuses);

  render();
  refreshStatuses();

  return () => {
    disposed = true;
    offState(); offUp(); offDown();
    if (activityRaf) cancelAnimationFrame(activityRaf);
    if (skippedActivityTimer) clearTimeout(skippedActivityTimer);
    closeActivityStream();
  };
}

// ---- helpers ----
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
function b64EncodeUnicode(text) {
  return btoa(unescape(encodeURIComponent(String(text))));
}
async function writeTextFile(api, absPath, text) {
  return api.t64("t64:exec", {
    command: writeFileFromB64Cmd(absPath, b64EncodeUnicode(text)),
  });
}
async function readTextFile(api, absPath) {
  const res = await api.t64("t64:exec", { command: readFileCmd(absPath) });
  return (res && typeof res.stdout === "string") ? res.stdout : "";
}
function joinProjectFile(projectPath, relPath) {
  const root = String(projectPath || "").replace(/[\\/]+$/, "");
  const rel = String(relPath || "").replace(/^[\\/]+/, "").replace(/[\\/]+/g, pathSepFor(root));
  return `${root}${pathSepFor(root)}${rel}`;
}
function pathSepFor(projectPath) {
  return /^[A-Za-z]:[\\/]/.test(String(projectPath || "")) || String(projectPath || "").includes("\\")
    ? "\\"
    : "/";
}
function normalizeStudioPath(path) {
  return String(path || "")
    .trim()
    .replace(/\\/g, "/")
    .replace(/^\/+|\/+$/g, "")
    .replace(/\/{2,}/g, "/");
}
function wallyTomlPathForFolder(projectPath, studioFolder) {
  const normalized = normalizeStudioPath(studioFolder || DEFAULT_WALLY_FOLDER);
  const parts = normalized.split("/").filter(Boolean);
  const parent = parts.length > 1 ? parts.slice(0, -1).join("/") : "";
  return joinProjectFile(projectPath, parent ? `${parent}/wally.toml` : "wally.toml");
}
function dirnamePath(path) {
  const s = String(path || "").replace(/[\\/]+$/, "");
  const i = Math.max(s.lastIndexOf("/"), s.lastIndexOf("\\"));
  return i >= 0 ? s.slice(0, i) : ".";
}
function wallyInstallCmd(cwd) {
  if (IS_WINDOWS) {
    return psEncodedCmd(
      `$aftmanBin = Join-Path $env:USERPROFILE '.aftman\\bin'; ` +
      `$env:PATH = "$aftmanBin;$env:LOCALAPPDATA\\aftman\\bin;$env:PATH"; ` +
      `Set-Location -LiteralPath ${psQuote(cwd)}; ` +
      `$wally = Get-Command wally -ErrorAction SilentlyContinue; ` +
      `if ($wally) { & $wally.Source install; exit $LASTEXITCODE }; ` +
      `$aftman = Get-Command aftman -ErrorAction SilentlyContinue; ` +
      `if ($aftman) { & $aftman.Source run wally install; exit $LASTEXITCODE }; ` +
      `Write-Error 'wally not found on PATH and aftman is not available'; exit 127`
    );
  }
  const script =
    `export PATH="$HOME/.aftman/bin:$HOME/.local/share/aftman/bin:$HOME/.wally/bin:$PATH"; ` +
    `cd ${shQuote(cwd)} && ` +
    `if command -v wally >/dev/null 2>&1; then wally install; ` +
    `elif command -v aftman >/dev/null 2>&1; then aftman run wally install; ` +
    `else echo "wally not found on PATH and aftman is not available" >&2; exit 127; fi`;
  return `if [ -x /bin/zsh ]; then /bin/zsh -lc ${shQuote(script)}; elif [ -x /bin/bash ]; then /bin/bash -lc ${shQuote(script)}; else /bin/sh -c ${shQuote(script)}; fi`;
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
  const normalized = normalizeStudioPath(path);
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
function renderActivityLine(entry) {
  const rendered = activitySummary(entry.frame);
  if (!rendered) return null;
  const card = document.createElement("article");
  card.className = `project-log-card ${rendered.cls}`;
  const meta = Array.isArray(rendered.meta) ? rendered.meta.filter(Boolean) : [];
  card.innerHTML =
    `<div class="project-log-card-head">` +
      `<span class="project-log-kind">${escapeHTML(rendered.kind)}</span>` +
      `<span class="project-log-time">${formatClock(entry.at)}</span>` +
    `</div>` +
    `<div class="project-log-title">${escapeHTML(rendered.title)}</div>` +
    (rendered.path ? (
      `<div class="project-log-path-row">` +
        `<div class="project-log-path">${escapeHTML(rendered.path)}</div>` +
        `<button class="project-log-copy" data-copy-path="${escapeHTML(rendered.path)}">Copy path</button>` +
      `</div>`
    ) : "") +
    (meta.length ? `<div class="project-log-meta">${meta.map((item) => `<span>${escapeHTML(item)}</span>`).join("")}</div>` : "");
  return card;
}
function activitySummary(frame) {
  if (!frame || typeof frame !== "object") return null;
  if (frame.type === "op") return activityOpSummary(frame);

  const t = String(frame.type || "event").toLowerCase();
  const cls = t.includes("error") || t.includes("conflict") ? "is-err"
    : t.includes("sync") ? "is-ok"
    : t.includes("plugin") ? "is-warn"
    : "is-info";
  let title = "";
  let path = "";
  const meta = [];
  if (t === "initial-choice-needed") title = "Initial sync needs a source choice";
  else if (t === "initial-choice-made") {
    title = "Initial sync choice applied";
    meta.push(`choice ${frame.choice || "?"}`);
  } else if (t === "config-changed") title = "Project config reloaded";
  else if (t === "conflict") {
    title = "Sync conflict detected";
    path = frame.path || "";
  } else if (t === "busy") {
    title = frame.message || frame.msg || "Daemon event burst collapsed";
    meta.push("log throttle");
  } else title = frame.message || frame.msg || JSON.stringify(frame);
  return { kind: t, cls, title, path, meta };
}
function activityOpSummary(frame) {
  const innerOp = frame && frame.op;
  if (!innerOp || typeof innerOp !== "object") return null;
  const kind = String(innerOp.op || "op").toLowerCase();
  const pathArr = Array.isArray(innerOp.path) ? innerOp.path : [];
  const pathStr = pathArr.join("/");
  const meta = ["filesystem watcher"];
  const node = innerOp.node && typeof innerOp.node === "object" ? innerOp.node : null;
  if (node && node.class) meta.push(`class ${node.class}`);
  if (kind === "rename") {
    const from = Array.isArray(innerOp.from) ? innerOp.from.join("/") : "?";
    const to = Array.isArray(innerOp.to) ? innerOp.to.join("/") : "?";
    return { kind, cls: "is-fs", title: "Renamed synced path", path: to, meta: [`from ${from}`] };
  }
  if (kind === "delete") return { kind, cls: "is-fs", title: "Deleted synced path", path: pathStr || "unknown path", meta };
  if (kind === "set") return { kind, cls: "is-fs", title: "Created or replaced synced path", path: pathStr || "unknown path", meta };
  if (kind === "update") return { kind, cls: "is-fs", title: "Updated synced path", path: pathStr || "unknown path", meta };
  return { kind, cls: "is-info", title: "Daemon operation", path: pathStr, meta: [JSON.stringify(innerOp)] };
}
function formatClock(ts) {
  const d = new Date(ts || Date.now());
  return d.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit", second: "2-digit" });
}
function countDupeGroups(tree) {
  if (!tree || typeof tree !== "object") return 0;
  const SUFFIX_RE = /\s\[\d+\]$/;
  let groups = 0;
  function visit(node) {
    if (!node || typeof node !== "object") return;
    const kids = Array.isArray(node) ? node
      : Array.isArray(node.children) ? node.children
      : Array.isArray(node.services) ? node.services
      : null;
    if (kids) {
      if (kids.some((c) => c && typeof c.name === "string" && SUFFIX_RE.test(c.name))) {
        groups++;
      }
      for (const c of kids) visit(c);
    }
  }
  visit(tree);
  return groups;
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
function folderSVG() {
  return '<svg viewBox="0 0 16 16" width="18" height="18" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linejoin="round" aria-hidden="true">' +
    '<path d="M1.75 4.25a1 1 0 0 1 1-1h3.5l1.5 1.5h6.5a1 1 0 0 1 1 1v6.5a1 1 0 0 1-1 1H2.75a1 1 0 0 1-1-1v-7z"/>' +
    '</svg>';
}
