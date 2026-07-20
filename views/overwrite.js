// views/overwrite.js — initial divergence resolver.
//
// Studio -> Disk is intentionally a single clean overwrite. Disk -> Studio is
// staged in a two-pane transfer view (disk source on the left, Studio
// destination on the right) and committed only when the user confirms.
import { installDocumentEscape } from "./runtime.js";
import { daemonJson } from "../bridge.js";
import {
  divergenceItems,
  itemClassLabel,
  itemStateLabel,
  selectedTransferPaths,
  splitDisplayPath,
  transferMeta,
  transferVerb,
} from "./initial-selection.js";
import {
  annotateLastEdited,
  formatRelativeEdited,
  projectMemoryKey,
  sortDivergenceItems,
} from "./last-edited.js";

const SORT_LABELS = { recent: "Recently edited", path: "A to Z", action: "Change type" };
const FILTERS = [
  { key: "all", label: "All" },
  { key: "create", label: "Only on disk", mark: "+" },
  { key: "overwrite", label: "Differs", mark: "~" },
  { key: "remove", label: "Missing on disk", mark: "−" },
];
const DRAG_THRESHOLD_PX = 6;

export function mountOverwriteModal(api) {
  const overlay = document.createElement("div");
  overlay.className = "modal-overlay initial-overlay";
  overlay.hidden = true;
  overlay.setAttribute("role", "dialog");
  overlay.setAttribute("aria-modal", "true");
  overlay.setAttribute("aria-labelledby", "ow-title");
  overlay.innerHTML = `
    <div class="modal-card initial-card" role="document" data-step="choice">
      <header class="initial-hero">
        <div class="initial-icon" aria-hidden="true">RS</div>
        <div class="initial-copy">
          <p class="initial-eyebrow">Initial divergence</p>
          <h2 class="modal-title" id="ow-title">Studio and disk are different</h2>
          <p class="modal-sub">Choose the source of truth. Nothing changes until you confirm.</p>
        </div>
      </header>

      <section class="initial-choice-step" data-step-panel="choice">
        <div class="initial-sources" aria-label="Initial sync sources">
          <article class="initial-source-card is-studio">
            <div class="initial-source-head">
              <span class="initial-source-dot" aria-hidden="true"></span>
              <strong>Studio</strong>
              <span>Current place</span>
            </div>
            <div class="initial-source-stats" data-studio-stats>—</div>
          </article>
          <div class="initial-divergence-rail" aria-hidden="true">
            <span data-difference-count>—</span>
            <i></i>
          </div>
          <article class="initial-source-card is-disk">
            <div class="initial-source-head">
              <span class="initial-source-dot" aria-hidden="true"></span>
              <strong>Disk</strong>
              <span>Local project</span>
            </div>
            <div class="initial-source-stats" data-disk-stats>—</div>
          </article>
        </div>

        <div class="initial-summary" data-summary>
          <div class="initial-summary-head">
            <span>Divergent synced paths</span>
            <span data-summary-total>—</span>
          </div>
          <div class="initial-summary-groups" data-summary-groups></div>
        </div>

        <div class="initial-choice-actions">
          <button class="initial-choice-button" type="button" data-act="studio">
            <span class="initial-choice-direction">Studio <b aria-hidden="true">→</b> Disk</span>
            <strong>Keep Studio</strong>
            <small>Cleanly overwrite the local synced tree.</small>
          </button>
          <button class="initial-choice-button is-primary" type="button" data-act="disk">
            <span class="initial-choice-direction">Disk <b aria-hidden="true">→</b> Studio</span>
            <strong>Keep Disk</strong>
            <small data-disk-hint>Review and stage individual disk changes.</small>
          </button>
          <button class="initial-cancel" type="button" data-act="cancel">Cancel</button>
        </div>
      </section>

      <section class="initial-transfer-step" data-step-panel="transfer" hidden>
        <div class="transfer-toolbar">
          <button type="button" class="transfer-back" data-act="back" aria-label="Back to source choice">← Back</button>
          <div class="transfer-toolbar-copy">
            <strong>Stage disk files for Studio</strong>
            <span>Staging makes the disk copy win — even if the file was last edited in Studio. Nothing applies until you confirm.</span>
          </div>
          <div class="transfer-toolbar-actions">
            <button type="button" data-act="all">Stage all</button>
          </div>
        </div>

        <div class="transfer-grid">
          <section class="transfer-pane is-disk" data-source-zone aria-label="Disk changes">
            <header class="transfer-pane-head">
              <div>
                <span class="transfer-pane-kicker">Source · on disk</span>
                <strong>Disk changes</strong>
              </div>
              <span data-disk-change-count>0 changes</span>
            </header>
            <div class="transfer-controls">
              <input type="search" data-search placeholder="Filter by name or path" aria-label="Filter disk changes">
              <label class="transfer-sort">
                <span>Sort</span>
                <select data-sort aria-label="Sort disk changes">
                  ${Object.entries(SORT_LABELS).map(([value, label]) => `<option value="${value}">${label}</option>`).join("")}
                </select>
              </label>
            </div>
            <div class="transfer-filters" role="group" aria-label="Filter by change type" data-filter-chips></div>
            <div class="transfer-list is-available" data-disk-list></div>
          </section>

          <div class="transfer-direction" aria-hidden="true">
            <span>→</span>
            <i></i>
          </div>

          <section class="transfer-pane is-studio" data-drop-zone aria-label="Staged for Studio">
            <header class="transfer-pane-head">
              <div>
                <span class="transfer-pane-kicker">Destination · applied on confirm</span>
                <strong>Studio</strong>
              </div>
              <span data-studio-queue-count aria-live="polite">Nothing staged</span>
            </header>
            <div class="transfer-list is-selected" data-selected-list></div>
          </section>
        </div>

        <footer class="transfer-footer">
          <p data-transfer-summary>Unstaged files keep their Studio version.</p>
          <div class="modal-actions">
            <button type="button" data-act="cancel">Cancel</button>
            <button type="button" class="primary" data-act="done" disabled>Move to Studio</button>
          </div>
        </footer>
      </section>

      <p class="modal-err" data-err hidden></p>
    </div>
  `;
  document.body.appendChild(overlay);

  const $card = overlay.querySelector(".initial-card");
  const $choiceStep = overlay.querySelector('[data-step-panel="choice"]');
  const $transferStep = overlay.querySelector('[data-step-panel="transfer"]');
  const $summaryTotal = overlay.querySelector("[data-summary-total]");
  const $summaryGroups = overlay.querySelector("[data-summary-groups]");
  const $studioStats = overlay.querySelector("[data-studio-stats]");
  const $diskStats = overlay.querySelector("[data-disk-stats]");
  const $differenceCount = overlay.querySelector("[data-difference-count]");
  const $diskHint = overlay.querySelector("[data-disk-hint]");
  const $diskList = overlay.querySelector("[data-disk-list]");
  const $selectedList = overlay.querySelector("[data-selected-list]");
  const $sourceZone = overlay.querySelector("[data-source-zone]");
  const $dropZone = overlay.querySelector("[data-drop-zone]");
  const $search = overlay.querySelector("[data-search]");
  const $sort = overlay.querySelector("[data-sort]");
  const $filterChips = overlay.querySelector("[data-filter-chips]");
  const $studioQueueCount = overlay.querySelector("[data-studio-queue-count]");
  const $diskChangeCount = overlay.querySelector("[data-disk-change-count]");
  const $transferSummary = overlay.querySelector("[data-transfer-summary]");
  const $done = overlay.querySelector('[data-act="done"]');
  const $all = overlay.querySelector('[data-act="all"]');
  const $err = overlay.querySelector("[data-err]");

  let currentChoiceId = null;
  let currentProjectId = null;
  let currentItems = [];
  let itemsByPath = new Map();
  let selected = new Set();
  let busy = false;
  let step = "choice";
  let sortMode = "path";
  let filterAction = "all";
  let searchText = "";

  function open(data) {
    if (!overlay.hidden && currentChoiceId && data.choiceId === currentChoiceId) return;
    currentChoiceId = data.choiceId || null;
    currentProjectId = data.projectId || null;
    const memoryKey = projectMemoryKey(data.projectPath, data.projectId);
    const edits = api.lastEdited?.forProject?.(memoryKey) || {};
    currentItems = annotateLastEdited(divergenceItems(data.comparison), edits);
    itemsByPath = new Map(currentItems.map((item) => [item.path, item]));
    selected = new Set();
    sortMode = currentItems.some((item) => item.editedAt) ? "recent" : "path";
    filterAction = "all";
    searchText = "";
    $search.value = "";
    $sort.value = sortMode;
    renderOverview(data);
    renderTransfer();
    showStep("choice");
    clearError();
    setBusy(false);
    overlay.hidden = false;
    if (!anotherModalOwnsInput()) overlay.querySelector('[data-act="disk"]')?.focus();
  }

  // App-level prompts (projects root, update confirm) render above this
  // overlay. While one is visible it owns focus and Escape.
  function anotherModalOwnsInput() {
    return [...document.querySelectorAll(".modal-overlay:not([hidden])")]
      .some((element) => element !== overlay);
  }

  function close() {
    overlay.hidden = true;
    currentChoiceId = null;
    currentProjectId = null;
    currentItems = [];
    itemsByPath = new Map();
    selected = new Set();
    showStep("choice");
    setBusy(false);
  }

  function showStep(next) {
    step = next;
    const transferring = next === "transfer";
    $choiceStep.hidden = transferring;
    $transferStep.hidden = !transferring;
    $card.dataset.step = next;
    $card.classList.toggle("is-transfer-step", transferring);
    clearError();
    requestAnimationFrame(() => {
      if (overlay.hidden || anotherModalOwnsInput()) return;
      (transferring ? $search : overlay.querySelector('[data-act="disk"]'))?.focus();
    });
  }

  function setBusy(value) {
    busy = value;
    $card.classList.toggle("is-busy", value);
    overlay.querySelectorAll("button, input, select").forEach((control) => {
      control.disabled = value;
    });
    if (!value) renderTransfer();
  }

  function clearError() {
    $err.hidden = true;
    $err.textContent = "";
  }

  function showError(message) {
    $err.hidden = false;
    $err.textContent = message;
  }

  function renderOverview(data) {
    $studioStats.textContent = formatStats(data.studioStats);
    $diskStats.textContent = formatStats(data.diskStats);
    const total = currentItems.length;
    $differenceCount.textContent = `${total} ${total === 1 ? "difference" : "differences"}`;
    $summaryTotal.textContent = `${total} ${total === 1 ? "path" : "paths"}`;
    $diskHint.textContent = total
      ? "Pick per file — staged files use the disk copy."
      : "Use the complete local synced tree.";
    $summaryGroups.innerHTML = overviewGroups(currentItems)
      .filter((group) => group.items.length)
      .map(renderOverviewGroup)
      .join("") || `<div class="initial-summary-fallback">Detailed path comparison is unavailable. Keeping Disk will use the complete local synced tree.</div>`;
  }

  function visibleItems() {
    let list = sortDivergenceItems(currentItems, sortMode);
    if (filterAction !== "all") list = list.filter((item) => item.action === filterAction);
    const query = searchText.trim().toLowerCase();
    if (query) list = list.filter((item) => item.path.toLowerCase().includes(query));
    return list;
  }

  function stagedItems() {
    return [...selected].map((path) => itemsByPath.get(path)).filter(Boolean);
  }

  function renderTransfer() {
    const visible = visibleItems();
    const staged = stagedItems();
    const filtered = filterAction !== "all" || searchText.trim() !== "";
    const unstagedVisible = visible.filter((item) => !selected.has(item.path));

    $diskChangeCount.textContent = filtered
      ? `${visible.length} of ${currentItems.length}`
      : `${currentItems.length} ${currentItems.length === 1 ? "change" : "changes"}`;
    $studioQueueCount.textContent = staged.length
      ? `${staged.length} staged`
      : "Nothing staged";

    $all.textContent = unstagedVisible.length
      ? `Stage ${filtered ? "shown" : "all"} (${unstagedVisible.length})`
      : `Unstage ${filtered ? "shown" : "all"}`;
    $all.disabled = busy || visible.length === 0;
    $done.disabled = busy || staged.length === 0;
    $done.textContent = staged.length ? `Move ${staged.length} to Studio` : "Move to Studio";
    $transferSummary.textContent = transferSummaryText(staged);

    $filterChips.innerHTML = FILTERS.map((filter) => {
      const count = filter.key === "all"
        ? currentItems.length
        : currentItems.filter((item) => item.action === filter.key).length;
      const active = filterAction === filter.key;
      return `
        <button type="button" class="transfer-chip${active ? " is-active" : ""} chip-${filter.key}"
          data-filter="${filter.key}" aria-pressed="${active}" ${busy || (!count && filter.key !== "all") ? "disabled" : ""}>
          ${filter.mark ? `<i aria-hidden="true">${filter.mark}</i>` : ""}${filter.label}
          <span>${count}</span>
        </button>`;
    }).join("");

    $diskList.innerHTML = visible.length
      ? visible.map((item) => renderDiskItem(item, selected.has(item.path))).join("")
      : `<div class="transfer-empty is-quiet"><strong>${currentItems.length ? "No matches" : "No disk changes"}</strong><small>${currentItems.length ? "Adjust the filter or search." : "Studio and disk agree on every synced path."}</small></div>`;
    $selectedList.innerHTML = staged.length
      ? staged.map(renderSelectedItem).join("")
      : `<div class="transfer-empty"><span aria-hidden="true">→</span><strong>Nothing staged yet</strong><small>Click or drag disk files to stage them here.</small></div>`;
  }

  function addPath(path) {
    if (busy || !itemsByPath.has(path) || selected.has(path)) return;
    selected.add(path);
    renderTransfer();
  }

  function removePath(path) {
    if (busy || !selected.has(path)) return;
    selected.delete(path);
    renderTransfer();
  }

  function togglePath(path) {
    if (busy || !itemsByPath.has(path)) return;
    if (selected.has(path)) selected.delete(path);
    else selected.add(path);
    renderTransfer();
  }

  async function submit(choice) {
    if (busy || !currentChoiceId) return;
    const base = api.getDaemonBase(currentProjectId);
    if (!base) {
      showError("Daemon offline — reconnect before resolving this divergence.");
      return;
    }
    const paths = choice === "disk" ? selectedTransferPaths(currentItems, selected) : null;
    if (choice === "disk" && currentItems.length > 0 && paths.length === 0) {
      showError("Stage at least one disk change before confirming.");
      return;
    }
    setBusy(true);
    try {
      const body = { choiceId: currentChoiceId, choice };
      if (choice === "disk" && currentItems.length > 0) body.paths = paths;
      const result = await daemonJson(base, "/initial-choice", {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify(body),
      });
      if (result && result.ok === false) throw new Error(result.error || "choice rejected");
      if (choice === "studio") api.toast?.("Studio will overwrite the local synced tree");
      else if (choice === "disk") api.toast?.(`${paths.length || "All"} disk changes queued for Studio`);
      else api.toast?.("Initial sync canceled");
      close();
    } catch (error) {
      setBusy(false);
      showError(`Failed: ${error.message}`);
    }
  }

  overlay.addEventListener("click", (event) => {
    if (busy) return;
    const action = event.target.closest("[data-act]")?.dataset.act;
    if (action === "studio" || action === "cancel") void submit(action);
    if (action === "disk") {
      if (currentItems.length) showStep("transfer");
      else void submit("disk");
    }
    if (action === "back") showStep("choice");
    if (action === "done") void submit("disk");
    if (action === "all") {
      const visible = visibleItems();
      const unstaged = visible.filter((item) => !selected.has(item.path));
      if (unstaged.length) unstaged.forEach((item) => selected.add(item.path));
      else visible.forEach((item) => selected.delete(item.path));
      renderTransfer();
      return;
    }
    const filter = event.target.closest("[data-filter]")?.dataset.filter;
    if (filter) {
      filterAction = filter;
      renderTransfer();
      return;
    }
    const remove = event.target.closest("[data-remove-path]")?.dataset.removePath;
    if (remove) {
      removePath(remove);
      return;
    }
    const toggle = event.target.closest("[data-toggle-path]")?.dataset.togglePath;
    if (toggle) togglePath(toggle);
  });

  overlay.addEventListener("keydown", (event) => {
    if (busy || (event.key !== "Enter" && event.key !== " ")) return;
    const row = event.target.closest?.("[data-toggle-path]");
    if (!row) return;
    event.preventDefault();
    togglePath(row.dataset.togglePath);
    // Re-render replaced the row node; keep keyboard position on its successor.
    overlay.querySelector(`[data-toggle-path="${cssEscape(row.dataset.togglePath)}"]`)?.focus();
  });

  $search.addEventListener("input", () => {
    searchText = $search.value;
    renderTransfer();
  });
  $search.addEventListener("keydown", (event) => {
    if (event.key === "Escape" && $search.value) {
      event.stopPropagation();
      $search.value = "";
      searchText = "";
      renderTransfer();
    }
  });
  $sort.addEventListener("change", () => {
    sortMode = $sort.value;
    renderTransfer();
  });

  // Pointer-based drag. HTML5 drag events never reach the page inside the
  // desktop app — Tauri's native drag-drop interception (kept on for the
  // projects view's folder drops) swallows them — so rows are dragged with
  // pointer capture instead, which behaves identically in both hosts.
  let drag = null;

  function beginPointerDrag(event, row, fromList) {
    if (busy || drag || event.button !== 0) return;
    const path = fromList === "disk" ? row.dataset.dragPath : row.dataset.unstagePath;
    if (!path || (fromList === "disk" && selected.has(path))) return;
    drag = {
      path,
      fromList,
      row,
      pointerId: event.pointerId,
      startX: event.clientX,
      startY: event.clientY,
      active: false,
      over: false,
      ghost: null,
      grabDX: 0,
      grabDY: 0,
    };
    try { row.setPointerCapture(event.pointerId); } catch {}
  }

  function activatePointerDrag(event) {
    const rect = drag.row.getBoundingClientRect();
    drag.grabDX = drag.startX - rect.left;
    drag.grabDY = drag.startY - rect.top;
    const ghost = drag.row.cloneNode(true);
    ghost.className = `${drag.row.className} transfer-drag-ghost`;
    ghost.style.width = `${rect.width}px`;
    overlay.appendChild(ghost);
    drag.ghost = ghost;
    drag.row.classList.add("is-dragging");
    drag.active = true;
    positionDragGhost(event);
  }

  function positionDragGhost(event) {
    drag.ghost.style.left = `${event.clientX - drag.grabDX}px`;
    drag.ghost.style.top = `${event.clientY - drag.grabDY}px`;
  }

  function dragTargetPane() {
    return drag.fromList === "disk" ? $dropZone : $sourceZone;
  }

  function cleanupPointerDrag() {
    if (!drag) return;
    drag.ghost?.remove();
    drag.row.classList.remove("is-dragging");
    dragTargetPane().classList.remove("is-drag-over");
    drag = null;
  }

  $diskList.addEventListener("pointerdown", (event) => {
    if (event.target.closest("button")) return;
    const row = event.target.closest("[data-drag-path]");
    if (row) beginPointerDrag(event, row, "disk");
  });
  $selectedList.addEventListener("pointerdown", (event) => {
    if (event.target.closest("button")) return;
    const row = event.target.closest("[data-unstage-path]");
    if (row) beginPointerDrag(event, row, "staged");
  });
  overlay.addEventListener("pointermove", (event) => {
    if (!drag || event.pointerId !== drag.pointerId) return;
    if (!drag.active) {
      const moved = Math.hypot(event.clientX - drag.startX, event.clientY - drag.startY);
      if (moved < DRAG_THRESHOLD_PX) return;
      activatePointerDrag(event);
    }
    positionDragGhost(event);
    const under = document.elementFromPoint(event.clientX, event.clientY);
    drag.over = !!under && dragTargetPane().contains(under);
    dragTargetPane().classList.toggle("is-drag-over", drag.over);
  });
  overlay.addEventListener("pointerup", (event) => {
    if (!drag || event.pointerId !== drag.pointerId) return;
    const { active, over, path, fromList } = drag;
    cleanupPointerDrag();
    if (!active) return;
    // The click that may follow this pointerup would toggle the path a second
    // time. It fires in the same task when it fires at all (a removed row
    // produces none), so trap it for exactly one tick.
    const squelch = (clickEvent) => {
      clickEvent.stopPropagation();
      clickEvent.preventDefault();
    };
    document.addEventListener("click", squelch, { capture: true, once: true });
    setTimeout(() => document.removeEventListener("click", squelch, { capture: true }), 0);
    if (over) {
      if (fromList === "disk") addPath(path);
      else removePath(path);
    }
  });
  overlay.addEventListener("pointercancel", (event) => {
    if (drag && event.pointerId === drag.pointerId) cleanupPointerDrag();
  });

  installDocumentEscape((event) => {
    if (overlay.hidden || busy || anotherModalOwnsInput()) return;
    event.preventDefault();
    if (drag) {
      cleanupPointerDrag();
      return;
    }
    if (step === "transfer") showStep("choice");
    else void submit("cancel");
  });

  api.onBus("initial-choice-needed", (data) => {
    if (data && typeof data === "object") open(data);
  });
  api.onBus("initial-choice-made", (data) => {
    if (!data || typeof data !== "object" || !currentChoiceId) return;
    if (data.projectId && currentProjectId && data.projectId !== currentProjectId) return;
    if (!data.choiceId || data.choiceId === currentChoiceId) {
      api.toast?.("Initial sync resolved elsewhere");
      close();
    }
  });
}

function transferSummaryText(staged) {
  if (!staged.length) return "Unstaged files keep their Studio version.";
  const counts = { create: 0, overwrite: 0, remove: 0 };
  for (const item of staged) counts[item.action] = (counts[item.action] || 0) + 1;
  const parts = [
    counts.create && `${counts.create} create`,
    counts.overwrite && `${counts.overwrite} overwrite`,
    counts.remove && `${counts.remove} remove`,
  ].filter(Boolean);
  return `${parts.join(" · ")} — applied together, one Studio undo reverses it.`;
}

function overviewGroups(items) {
  return [
    { title: "Only on disk", hint: "missing in Studio", mark: "+", cls: "is-new", items: items.filter((item) => item.action === "create") },
    { title: "Differs", hint: "changed in Studio or on disk", mark: "~", cls: "is-changed", items: items.filter((item) => item.action === "overwrite") },
    { title: "Missing on disk", hint: "only in Studio", mark: "−", cls: "is-removed", items: items.filter((item) => item.action === "remove") },
  ];
}

function renderOverviewGroup(group) {
  const visible = group.items.slice(0, 6);
  const more = group.items.length - visible.length;
  return `
    <section class="initial-summary-group">
      <div class="initial-summary-label"><span>${escape(group.title)}</span><span>${group.items.length} · ${escape(group.hint)}</span></div>
      <ul>
        ${visible.map((item) => `<li class="${group.cls}"><span class="initial-summary-mark">${group.mark}</span>${renderPathSpan(item.path, "initial-summary-path")}<span class="initial-summary-meta">${escape(metaWithTime(item))}</span></li>`).join("")}
        ${more > 0 ? `<li class="initial-summary-more">+${more} more</li>` : ""}
      </ul>
    </section>`;
}

function renderPathSpan(path, className) {
  const { parent, name } = splitDisplayPath(path);
  return `<span class="${className}" title="${escape(path)}">${parent ? `<span class="path-parent">${escape(parent)}/</span>` : ""}<span class="path-name">${escape(name)}</span></span>`;
}

function renderDiskItem(item, staged) {
  const edited = formatRelativeEdited(item.editedAt);
  return `
    <article class="transfer-file action-${item.action}${staged ? " is-staged" : ""}"
      data-drag-path="${escape(item.path)}"
      data-toggle-path="${escape(item.path)}" role="button" tabindex="0" aria-pressed="${staged}"
      aria-label="${staged ? "Unstage" : "Stage"} ${escape(item.path)}">
      <span class="transfer-file-mark" aria-hidden="true">${markFor(item.action)}</span>
      <span class="transfer-file-copy">
        ${renderPathSpan(item.path, "transfer-file-path")}
        <small>${escape(diskItemMeta(item))}</small>
      </span>
      <span class="transfer-file-side">
        ${edited ? `<time title="Last synced edit ${escape(new Date(item.editedAt).toLocaleString())}">${escape(edited)}</time>` : ""}
        <span class="transfer-file-state" aria-hidden="true">${staged ? "✓" : "+"}</span>
      </span>
    </article>`;
}

// Line two of a source row describes the STATE of the divergence, never a
// direction the compare cannot know. The staging verb carries the action.
function diskItemMeta(item) {
  const state = itemStateLabel(item);
  if (item.classChanged) {
    return `${state} · ${item.studioClass || "Studio type"} → ${item.localClass || itemClassLabel(item)}`;
  }
  const cls = itemClassLabel(item);
  return `${state} · ${item.kind === "folder" ? `${cls} tree` : cls}`;
}

function renderSelectedItem(item) {
  return `
    <article class="transfer-file is-selected action-${item.action}" data-unstage-path="${escape(item.path)}">
      <span class="transfer-file-mark" aria-hidden="true">${markFor(item.action)}</span>
      <span class="transfer-file-copy">
        <span class="transfer-file-top">${renderPathSpan(item.path, "transfer-file-path")}</span>
        <small>${escape(transferVerb(item))}</small>
      </span>
      <button type="button" data-remove-path="${escape(item.path)}" aria-label="Unstage ${escape(item.path)}">✕</button>
    </article>`;
}

function markFor(action) {
  return action === "create" ? "+" : action === "remove" ? "−" : "~";
}

function metaWithTime(item) {
  const edited = formatRelativeEdited(item.editedAt);
  return edited ? `${transferMeta(item)} · ${edited}` : transferMeta(item);
}

function formatStats(stats) {
  const scripts = Number(stats?.scriptCount) || 0;
  const instances = Number(stats?.instanceCount) || 0;
  return `${scripts} scripts · ${instances} synced instances`;
}

function cssEscape(value) {
  return typeof CSS !== "undefined" && CSS.escape ? CSS.escape(value) : String(value).replace(/"/g, '\\"');
}

function escape(value) {
  return String(value ?? "")
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;")
    .replace(/'/g, "&#39;");
}
