// views/conflicts.js — list conflicts, show side-by-side diff, resolve.
import { daemonJson } from "../bridge.js";

// Uses jsdiff from CDN (loaded lazily so the widget still boots offline).
const DIFF_CDN = "https://cdn.jsdelivr.net/npm/diff@5.2.0/+esm";
let diffLib = null;

async function loadDiff() {
  if (diffLib) return diffLib;
  try {
    diffLib = await import(/* @vite-ignore */ DIFF_CDN);
  } catch (e) {
    console.warn("diff lib failed to load", e);
    diffLib = null;
  }
  return diffLib;
}

export function mountConflicts(root, api) {
  root.innerHTML = `
    <div class="row" style="margin-bottom:8px; justify-content:space-between">
      <div class="row">
        <button id="cf-reload">Refresh</button>
        <span id="cf-count" style="color:var(--muted)">—</span>
      </div>
      <div class="row">
        <button id="cf-all-local">Keep all local</button>
        <button id="cf-all-studio">Keep all studio</button>
      </div>
    </div>
    <div id="cf-list"></div>
    <div id="cf-empty" class="empty" hidden>No conflicts. 🎉</div>
  `;

  const $list = root.querySelector("#cf-list");
  const $empty = root.querySelector("#cf-empty");
  const $count = root.querySelector("#cf-count");
  const $reload = root.querySelector("#cf-reload");
  const $allLocal = root.querySelector("#cf-all-local");
  const $allStudio = root.querySelector("#cf-all-studio");

  let conflicts = [];

  function updateBadge(n) {
    const badge = document.getElementById("conflicts-badge");
    if (!badge) return;
    if (n > 0) { badge.hidden = false; badge.textContent = String(n); }
    else badge.hidden = true;
  }

  async function load() {
    const base = api.getDaemonBase();
    const s = api.getState();
    const proj = (s.projects || []).find((p) => p.id === s.activeProjectId);
    if (!base) { $count.textContent = "daemon offline"; return; }
    if (!proj) { $count.textContent = "no active project"; $list.innerHTML = ""; $empty.hidden = true; return; }
    try {
      const data = await daemonJson(base, "/resolve");
      conflicts = Array.isArray(data) ? data : (data.conflicts || []);
      render();
    } catch (e) {
      $count.textContent = `error: ${e.message}`;
    }
  }

  async function render() {
    $list.innerHTML = "";
    $count.textContent = `${conflicts.length} conflict(s)`;
    updateBadge(conflicts.length);
    $empty.hidden = conflicts.length > 0;
    if (!conflicts.length) return;

    const lib = await loadDiff();
    for (const c of conflicts) {
      const card = document.createElement("div");
      card.className = "conflict";
      card.innerHTML = `
        <div class="conflict-head">
          <span class="path"></span>
          <div class="actions">
            <button data-act="local">Keep Local</button>
            <button data-act="studio">Keep Studio</button>
          </div>
        </div>
        <div class="diff">
          <div class="diff-pane"><h4>Local (FS)</h4><div class="body" data-side="local"></div></div>
          <div class="diff-pane"><h4>Studio</h4><div class="body" data-side="studio"></div></div>
        </div>
      `;
      card.querySelector(".path").textContent = c.path || c.id || "(unnamed)";
      const left = card.querySelector('[data-side="local"]');
      const right = card.querySelector('[data-side="studio"]');
      renderDiff(lib, left, right, c.local ?? "", c.studio ?? "");
      card.querySelector('[data-act="local"]').addEventListener("click", () => resolve(c, "local"));
      card.querySelector('[data-act="studio"]').addEventListener("click", () => resolve(c, "studio"));
      $list.appendChild(card);
    }
  }

  function renderDiff(lib, leftEl, rightEl, local, studio) {
    if (!lib || !lib.diffLines) {
      leftEl.textContent = local;
      rightEl.textContent = studio;
      return;
    }
    const parts = lib.diffLines(local, studio);
    const leftFrag = document.createDocumentFragment();
    const rightFrag = document.createDocumentFragment();
    for (const p of parts) {
      if (p.added) {
        rightFrag.appendChild(span("add", p.value));
      } else if (p.removed) {
        leftFrag.appendChild(span("rm", p.value));
      } else {
        leftFrag.appendChild(document.createTextNode(p.value));
        rightFrag.appendChild(document.createTextNode(p.value));
      }
    }
    leftEl.appendChild(leftFrag);
    rightEl.appendChild(rightFrag);
  }

  async function resolve(conflict, side) {
    const base = api.getDaemonBase();
    const s = api.getState();
    const proj = (s.projects || []).find((p) => p.id === s.activeProjectId);
    if (!base || !proj) return;
    try {
      await daemonJson(base, "/resolve", {
        method: "POST",
        body: JSON.stringify({
          id: conflict.id,
          path: conflict.path,
          keep: side,
          choice: side,
        }),
      });
      api.toast(`Kept ${side}`);
      conflicts = conflicts.filter((c) => c !== conflict);
      render();
    } catch (e) {
      api.toast(`resolve failed: ${e.message}`);
    }
  }

  async function resolveAll(side) {
    if (!conflicts.length) return;
    for (const c of [...conflicts]) await resolve(c, side);
  }

  $reload.addEventListener("click", load);
  $allLocal.addEventListener("click", () => resolveAll("local"));
  $allStudio.addEventListener("click", () => resolveAll("studio"));

  const offUp = api.onBus("daemon:up", load);
  const offDown = api.onBus("daemon:down", () => { $count.textContent = "daemon offline"; });
  const offState = api.onBus("state", load);

  load();

  return () => { offUp(); offDown(); offState(); };
}

function span(cls, text) {
  const el = document.createElement("span");
  el.className = cls;
  el.textContent = text;
  return el;
}
