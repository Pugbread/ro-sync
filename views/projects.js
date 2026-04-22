// views/projects.js — saved project list with add/edit/remove + status dots.
import { daemonJson } from "../bridge.js";
import { pickFolderCmd } from "../platform.js";

export function mountProjects(root, api) {
  root.innerHTML = `
    <div class="projects-header">
      <input id="proj-path" type="text" placeholder="/absolute/path/to/project" spellcheck="false" />
      <button id="proj-pick" title="Pick folder">Browse…</button>
      <button id="proj-add" class="primary">Add</button>
    </div>
    <div class="projects-header projects-header-2">
      <input id="proj-game-id" type="text" placeholder="Game ID (optional, e.g. 1234567890)" spellcheck="false" inputmode="numeric" />
      <input id="proj-place-ids" type="text" placeholder="Place IDs (optional, comma-separated)" spellcheck="false" />
    </div>
    <ul id="proj-list" class="project-list"></ul>
    <div id="proj-empty" class="empty" hidden>No projects yet. Add one above.</div>
  `;

  const $path = root.querySelector("#proj-path");
  const $pick = root.querySelector("#proj-pick");
  const $add = root.querySelector("#proj-add");
  const $gameId = root.querySelector("#proj-game-id");
  const $placeIds = root.querySelector("#proj-place-ids");
  const $list = root.querySelector("#proj-list");
  const $empty = root.querySelector("#proj-empty");

  const state = api.getState();
  let snapshotByProject = new Map();  // projectId -> {ok,lastSync,pending}
  let editingId = null;

  function parsePlaceIds(raw) {
    if (!raw) return [];
    return String(raw)
      .split(",")
      .map((s) => s.trim())
      .filter(Boolean);
  }

  function render() {
    const s = api.getState();
    const projects = s.projects || [];
    $list.innerHTML = "";
    if (!projects.length) { $empty.hidden = false; return; }
    $empty.hidden = true;
    for (const p of projects) {
      const li = document.createElement("li");
      li.className = "project";
      if (p.id === s.activeProjectId) li.setAttribute("aria-current", "true");
      const st = snapshotByProject.get(p.id) || {};
      const dotCls = st.ok === true ? "dot-ok" : st.ok === false ? "dot-err" : "dot-idle";
      const placeIdsStr = Array.isArray(p.placeIds) ? p.placeIds.join(", ") : "";
      const isServing = p.id === s.activeProjectId;
      li.innerHTML = `
        <span class="dot ${dotCls}" title="${escapeAttr(st.label || "idle")}"></span>
        <div class="meta">
          <span class="name"></span>
          <span class="path"></span>
          <span class="roblox-ids" hidden></span>
          <span class="dupe-note" hidden></span>
        </div>
        <div class="actions">
          <label class="switch" title="${isServing ? "Stop serving" : "Start serving"}" data-act="serve-wrap">
            <input type="checkbox" data-act="serve" ${isServing ? "checked" : ""} aria-label="Serve this project" />
            <span class="switch-track"><span class="switch-thumb"></span></span>
          </label>
          <button data-act="edit" title="Edit GameId / PlaceIds" aria-label="Edit">${pencilSVG()}</button>
        </div>
      `;
      li.querySelector(".name").textContent = p.name || basename(p.path);
      li.querySelector(".path").textContent = p.path;
      const $ids = li.querySelector(".roblox-ids");
      const idBits = [];
      if (p.gameId) idBits.push(`game: ${p.gameId}`);
      if (placeIdsStr) idBits.push(`places: ${placeIdsStr}`);
      if (idBits.length) {
        $ids.textContent = idBits.join("  ·  ");
        $ids.hidden = false;
      }
      const groups = st.dupeGroups;
      if (typeof groups === "number" && groups > 0) {
        const $dupe = li.querySelector(".dupe-note");
        $dupe.textContent = `${groups} duplicate-name group${groups === 1 ? "" : "s"}`;
        $dupe.hidden = false;
      }
      const $serve = li.querySelector('[data-act="serve"]');
      $serve.addEventListener("click", (e) => e.stopPropagation());
      $serve.addEventListener("change", (e) => {
        e.stopPropagation();
        if (e.target.checked) serve(p.id);
        else stopServing(p.id);
      });
      li.querySelector('[data-act="serve-wrap"]').addEventListener("click", (e) => e.stopPropagation());
      li.querySelector('[data-act="edit"]').addEventListener("click", (e) => {
        e.stopPropagation();
        startEdit(p.id, li);
      });
      $list.appendChild(li);

      if (editingId === p.id) renderEditForm(p, li);
    }
  }

  function renderEditForm(p, li) {
    const form = document.createElement("div");
    form.className = "project-edit";
    form.innerHTML = `
      <label>Game ID
        <input type="text" data-field="gameId" spellcheck="false" inputmode="numeric" />
      </label>
      <label>Place IDs (comma-separated)
        <input type="text" data-field="placeIds" spellcheck="false" />
      </label>
      <div class="row project-edit-actions">
        <button class="primary" data-act="save">Save</button>
        <button data-act="cancel">Cancel</button>
        <span class="spacer"></span>
        <button data-act="remove" class="danger">Delete project</button>
      </div>
    `;
    const $g = form.querySelector('[data-field="gameId"]');
    const $pl = form.querySelector('[data-field="placeIds"]');
    $g.value = p.gameId || "";
    $pl.value = Array.isArray(p.placeIds) ? p.placeIds.join(", ") : "";
    form.addEventListener("click", (e) => e.stopPropagation());
    form.querySelector('[data-act="save"]').addEventListener("click", () => {
      saveEdit(p.id, $g.value.trim(), parsePlaceIds($pl.value));
    });
    form.querySelector('[data-act="cancel"]').addEventListener("click", () => {
      editingId = null;
      render();
    });
    // Two-click confirm: first click swaps the button into "Really delete?"
    // state (the sandboxed widget doesn't support window.confirm, which was
    // silently cancelling every delete).
    const $remove = form.querySelector('[data-act="remove"]');
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
    li.appendChild(form);
  }

  function startEdit(id, li) {
    editingId = editingId === id ? null : id;
    render();
  }

  async function saveEdit(id, gameId, placeIds) {
    const s = api.getState();
    const prev = (s.projects || []).find((p) => p.id === id);
    const prevGameId = (prev && prev.gameId) || null;
    const prevPlaceIds = (prev && Array.isArray(prev.placeIds)) ? prev.placeIds.join(",") : "";
    const nextGameId = gameId || null;
    const nextPlaceIdsStr = placeIds.join(",");
    const changedLaunchArgs = (prevGameId !== nextGameId) || (prevPlaceIds !== nextPlaceIdsStr);

    const next = (s.projects || []).map((p) =>
      p.id === id ? { ...p, gameId: nextGameId, placeIds } : p
    );
    api.setState({ projects: next });
    editingId = null;
    render();

    if (id === s.activeProjectId && changedLaunchArgs) {
      // Daemon was launched with the old --game-id / --place-id CLI args.
      // Kill it so the state observer's ensureDaemon() relaunches with the
      // new args; otherwise the plugin keeps seeing the stale gameId and
      // reports "wrong game".
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
      placeIds: parsePlaceIds($placeIds.value),
    };
    const next = [...(s.projects || []), proj];
    // Don't auto-start serving — the switch on each row is the explicit control.
    api.setState({ projects: next });
    $path.value = "";
    $gameId.value = "";
    $placeIds.value = "";
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
    render();
  }

  // Serving is single-project: turning one ON turns any other OFF. The state
  // observer in app.js picks up the change and (re)launches the daemon.
  function serve(id) {
    api.setState({ activeProjectId: id });
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
      // Strip trailing slash/backslash; osascript and PS both round-trip with
      // a trailing sep on some paths.
      const out = (res?.stdout || "").trim().replace(/[\\/]+$/, "");
      if (out) {
        $path.value = out;
      } else if (res?.stderr && !/User canceled|cancelled/i.test(res.stderr)) {
        api.toast("Folder picker failed");
      }
    } catch (e) {
      api.toast("Folder picker unavailable");
    }
  }

  async function refreshStatuses() {
    const base = api.getDaemonBase();
    const s = api.getState();
    // Daemon is scoped to the currently-active project; we can only probe that one.
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

  $add.addEventListener("click", add);
  $pick.addEventListener("click", pickFolder);
  $path.addEventListener("keydown", (e) => { if (e.key === "Enter") add(); });
  $gameId.addEventListener("keydown", (e) => { if (e.key === "Enter") add(); });
  $placeIds.addEventListener("keydown", (e) => { if (e.key === "Enter") add(); });

  const offState = api.onBus("state", render);
  const offUp = api.onBus("daemon:up", refreshStatuses);
  const offDown = api.onBus("daemon:down", refreshStatuses);

  render();
  refreshStatuses();
  api.setStatus(`${(state.projects || []).length} project(s)`);

  return () => { offState(); offUp(); offDown(); };
}

function basename(p) {
  if (!p) return "";
  const s = p.replace(/\/+$/, "");
  const i = s.lastIndexOf("/");
  return i >= 0 ? s.slice(i + 1) : s;
}
function escapeAttr(s) {
  return String(s).replace(/[&<>"']/g, (c) => ({
    "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;",
  }[c]));
}
// Walk an /snapshot tree and count nodes whose children include at least one
// `[N]` suffix — i.e. a group of siblings that share a base name on disk.
// Defensive over shape: accepts `children` / `services` / array nodes.
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

function pencilSVG() {
  return '<svg viewBox="0 0 16 16" width="12" height="12" fill="none" stroke="currentColor" stroke-width="1.5" aria-hidden="true">' +
    '<path d="M2 12.5V14h1.5l8-8-1.5-1.5-8 8z"/>' +
    '<path d="M10.5 4.5l1-1 1.5 1.5-1 1"/>' +
    '</svg>';
}
