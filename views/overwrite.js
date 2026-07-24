// views/overwrite.js — initial divergence resolver.
//
// Studio -> Disk is intentionally a single clean overwrite. Disk -> Studio is
// staged in a two-pane transfer view (disk source on the left, Studio
// destination on the right) and committed only when the user confirms.
import { installDocumentEscape } from "./runtime.js";
import { daemonJson } from "../bridge.js";
import {
  INITIAL_DETAIL_PAGE_LIMIT,
  INITIAL_LIST_RENDER_LIMIT,
  acceptChoiceDetailPage,
  boundedRenderPair,
  choiceDetailsPath,
  choiceSummaryCounts,
  createChoiceDetailAccumulator,
  itemClassLabel,
  itemStateLabel,
  selectedTransferIds,
  selectionIdChunks,
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
            <span data-detail-progress role="status" aria-live="polite"></span>
          </div>
          <div class="transfer-toolbar-actions">
            <button type="button" data-act="retry-details" hidden>Retry list</button>
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
            <button type="button" data-act="all-disk">Move all disk changes</button>
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
  const $detailProgress = overlay.querySelector("[data-detail-progress]");
  const $retryDetails = overlay.querySelector('[data-act="retry-details"]');
  const $done = overlay.querySelector('[data-act="done"]');
  const $all = overlay.querySelector('[data-act="all"]');
  const $allDisk = overlay.querySelector('[data-act="all-disk"]');
  const $err = overlay.querySelector("[data-err]");

  let currentChoiceId = null;
  let currentProjectId = null;
  let currentOverviewData = {};
  let currentEdits = {};
  let currentItems = [];
  let itemsById = new Map();
  let selected = new Set();
  let detailAccumulator = null;
  let detailAbort = null;
  let detailGeneration = 0;
  let detailLoading = false;
  let detailError = null;
  let detailTotal = 0;
  let busy = false;
  let step = "choice";
  let sortMode = "path";
  let filterAction = "all";
  let searchText = "";
  let visibleCache = null;
  let actionCounts = { create: 0, overwrite: 0, remove: 0 };

  function open(data) {
    const choiceId = typeof data?.choiceId === "string" ? data.choiceId : null;
    if (!choiceId) return;
    if (!overlay.hidden && currentChoiceId === choiceId) {
      currentOverviewData = { ...currentOverviewData, ...data };
      const latestSummary = choiceSummaryCounts(currentOverviewData);
      if (
        latestSummary.total > 0
        && (!detailAccumulator
          || (detailAccumulator.totalCount === 0 && currentItems.length === 0))
      ) {
        initializeDetailState(latestSummary);
        void loadDetails();
      }
      renderOverview();
      renderDetailProgress();
      return;
    }

    abortDetailLoad();
    currentChoiceId = choiceId;
    currentProjectId = data.projectId || null;
    currentOverviewData = { ...data };
    const memoryKey = projectMemoryKey(data.projectPath, data.projectId);
    currentEdits = api.lastEdited?.forProject?.(memoryKey) || {};
    initializeDetailState(choiceSummaryCounts(data));
    selected = new Set();
    sortMode = "path";
    filterAction = "all";
    searchText = "";
    visibleCache = null;
    $search.value = "";
    $sort.value = sortMode;
    renderOverview();
    renderTransfer();
    showStep("choice");
    clearError();
    setBusy(false);
    overlay.hidden = false;
    if (!anotherModalOwnsInput()) overlay.querySelector('[data-act="disk"]')?.focus();
    if (detailTotal > 0) void loadDetails();
  }

  function initializeDetailState(summary) {
    currentItems = [];
    itemsById = new Map();
    visibleCache = null;
    detailTotal = summary.total;
    detailAccumulator = createChoiceDetailAccumulator(currentChoiceId, detailTotal);
    detailLoading = false;
    detailError = null;
    actionCounts = {
      create: summary.create,
      overwrite: summary.overwrite,
      remove: summary.remove,
    };
  }

  async function loadDetails({ restart = false } = {}) {
    if (!currentChoiceId || detailTotal === 0) {
      renderDetailProgress();
      return;
    }
    const base = api.getDaemonBase(currentProjectId);
    if (!base) {
      detailError = "Daemon offline — reconnect to load individual paths.";
      renderDetailProgress();
      renderTransfer();
      return;
    }
    if (detailLoading) return;
    if (restart || !detailAccumulator || detailAccumulator.complete) {
      if (!restart && detailAccumulator?.complete) return;
      selected = new Set();
      initializeDetailState(choiceSummaryCounts(currentOverviewData));
    }

    abortDetailLoad();
    const controller = new AbortController();
    detailAbort = controller;
    const generation = ++detailGeneration;
    const choiceId = currentChoiceId;
    detailLoading = true;
    detailError = null;
    renderDetailProgress();
    renderTransfer();

    try {
      while (!detailAccumulator.complete) {
        const page = await daemonJson(
          base,
          choiceDetailsPath(
            choiceId,
            detailAccumulator.cursor,
            INITIAL_DETAIL_PAGE_LIMIT,
          ),
          { signal: controller.signal },
        );
        if (generation !== detailGeneration || choiceId !== currentChoiceId) return;
        const added = acceptChoiceDetailPage(detailAccumulator, page);
        const annotated = annotateLastEdited(added, currentEdits);
        currentItems.push(...annotated);
        for (const item of annotated) itemsById.set(item.id, item);
        renderDetailProgress();
      }

      detailLoading = false;
      detailAbort = null;
      detailError = null;
      actionCounts = { create: 0, overwrite: 0, remove: 0 };
      for (const item of currentItems) {
        actionCounts[item.action] = (actionCounts[item.action] || 0) + 1;
      }
      sortMode = currentItems.some((item) => item.editedAt) ? "recent" : "path";
      $sort.value = sortMode;
      visibleCache = null;
      renderOverview();
      renderDetailProgress();
      renderTransfer();
    } catch (error) {
      if (controller.signal.aborted || generation !== detailGeneration) return;
      detailLoading = false;
      detailAbort = null;
      detailError = error?.message || "Could not load individual paths.";
      renderOverview();
      renderDetailProgress();
      renderTransfer();
    }
  }

  function abortDetailLoad() {
    detailGeneration += 1;
    if (detailAbort) detailAbort.abort();
    detailAbort = null;
    detailLoading = false;
  }

  // App-level prompts (projects root, update confirm) render above this
  // overlay. While one is visible it owns focus and Escape.
  function anotherModalOwnsInput() {
    return [...document.querySelectorAll(".modal-overlay:not([hidden])")]
      .some((element) => element !== overlay);
  }

  function close() {
    abortDetailLoad();
    overlay.hidden = true;
    currentChoiceId = null;
    currentProjectId = null;
    currentOverviewData = {};
    currentEdits = {};
    currentItems = [];
    itemsById = new Map();
    selected = new Set();
    detailAccumulator = null;
    detailError = null;
    detailTotal = 0;
    visibleCache = null;
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

  function renderOverview() {
    $studioStats.textContent = formatStats(currentOverviewData.studioStats);
    $diskStats.textContent = formatStats(currentOverviewData.diskStats);
    const total = detailTotal;
    $differenceCount.textContent = `${total} ${total === 1 ? "difference" : "differences"}`;
    $summaryTotal.textContent = `${total} ${total === 1 ? "path" : "paths"}`;
    $diskHint.textContent = total
      ? detailError
        ? "Individual review is unavailable; complete Studio and Disk choices remain safe."
        : detailAccumulator?.complete
        ? "Pick per file — staged files use the disk copy."
        : "Review individual changes as the bounded file list loads."
      : "Use the complete local synced tree.";
    $summaryGroups.innerHTML = detailAccumulator?.complete
      ? overviewGroups(currentItems)
        .filter((group) => group.items.length)
        .map(renderOverviewGroup)
        .join("")
      : renderOverviewCounts(actionCounts);
    if (!$summaryGroups.innerHTML) {
      $summaryGroups.innerHTML = total
        ? `<div class="initial-summary-fallback">${total} divergent paths reported. Individual details are loading separately.</div>`
        : `<div class="initial-summary-fallback">No divergent paths were reported. Keeping Disk will use the complete local synced tree.</div>`;
    }
  }

  function renderDetailProgress() {
    const loaded = currentItems.length;
    if (detailError) {
      $detailProgress.textContent = `Individual path list unavailable (${loaded} of ${detailTotal} loaded). You can still keep all of Studio or all of Disk.`;
      $retryDetails.hidden = false;
    } else if (detailLoading) {
      $detailProgress.textContent = `Loading individual paths… ${loaded} of ${detailTotal}`;
      $retryDetails.hidden = true;
    } else if (detailAccumulator?.complete) {
      $detailProgress.textContent = `${detailTotal} individual ${detailTotal === 1 ? "path" : "paths"} ready`;
      $retryDetails.hidden = true;
    } else {
      $detailProgress.textContent = detailTotal
        ? `Waiting to load ${detailTotal} individual paths`
        : "No individual paths to review";
      $retryDetails.hidden = true;
    }
    $retryDetails.disabled = busy || detailLoading;
    $allDisk.disabled = busy;
  }

  function visibleItems() {
    if (visibleCache) return visibleCache;
    let list = sortDivergenceItems(currentItems, sortMode);
    if (filterAction !== "all") list = list.filter((item) => item.action === filterAction);
    const query = searchText.trim().toLowerCase();
    if (query) list = list.filter((item) => item.path.toLowerCase().includes(query));
    visibleCache = list;
    return visibleCache;
  }

  function stagedItems() {
    return [...selected]
      .sort((left, right) => left - right)
      .map((id) => itemsById.get(id))
      .filter(Boolean);
  }

  function renderTransfer() {
    const detailReady = detailAccumulator?.complete === true && !detailError;
    const visible = visibleItems();
    const staged = stagedItems();
    const filtered = filterAction !== "all" || searchText.trim() !== "";
    const unstagedVisible = visible.filter((item) => !selected.has(item.id));
    const overviewRows = detailAccumulator?.complete
      ? Math.min(actionCounts.create || 0, 6)
        + Math.min(actionCounts.overwrite || 0, 6)
        + Math.min(actionCounts.remove || 0, 6)
      : 0;
    const transferRowBudget = Math.max(0, INITIAL_LIST_RENDER_LIMIT - overviewRows);
    const windows = boundedRenderPair(visible, staged, transferRowBudget);
    const diskWindow = windows.available;
    const stagedWindow = windows.staged;

    $diskChangeCount.textContent = filtered
      ? `${visible.length} of ${currentItems.length}`
      : detailReady
        ? `${currentItems.length} ${currentItems.length === 1 ? "change" : "changes"}`
        : `${currentItems.length} of ${detailTotal} loaded`;
    $studioQueueCount.textContent = staged.length
      ? `${staged.length} staged`
      : "Nothing staged";

    $all.textContent = unstagedVisible.length
      ? `Stage ${filtered ? "shown" : "all"} (${unstagedVisible.length})`
      : `Unstage ${filtered ? "shown" : "all"}`;
    $all.disabled = busy || !detailReady || visible.length === 0;
    $done.disabled = busy || !detailReady || staged.length === 0;
    $done.textContent = staged.length ? `Move ${staged.length} to Studio` : "Move to Studio";
    $transferSummary.textContent = transferSummaryText(staged);
    $search.disabled = busy || !detailReady;
    $sort.disabled = busy || !detailReady;
    $allDisk.disabled = busy;
    renderDetailProgress();

    $filterChips.innerHTML = FILTERS.map((filter) => {
      const count = filter.key === "all"
        ? detailTotal
        : actionCounts[filter.key] || 0;
      const active = filterAction === filter.key;
      return `
        <button type="button" class="transfer-chip${active ? " is-active" : ""} chip-${filter.key}"
          data-filter="${filter.key}" aria-pressed="${active}" ${busy || !detailReady || (!count && filter.key !== "all") ? "disabled" : ""}>
          ${filter.mark ? `<i aria-hidden="true">${filter.mark}</i>` : ""}${filter.label}
          <span>${count}</span>
        </button>`;
    }).join("");

    $diskList.innerHTML = visible.length
      ? diskWindow.items.map((item) => renderDiskItem(item, selected.has(item.id))).join("")
        + renderListOverflow(diskWindow.hidden, "more matching paths", "Filter by name or change type to narrow the list.")
      : renderAvailableEmpty({
        detailReady,
        detailLoading,
        detailError,
        detailTotal,
        loaded: currentItems.length,
      });
    $selectedList.innerHTML = staged.length
      ? stagedWindow.items.map(renderSelectedItem).join("")
        + renderListOverflow(stagedWindow.hidden, "more staged paths", "All staged paths will still be applied on confirm.")
      : `<div class="transfer-empty"><span aria-hidden="true">→</span><strong>Nothing staged yet</strong><small>Click or drag disk files to stage them here.</small></div>`;
  }

  function addItem(rawId) {
    const id = uiItemId(rawId);
    if (busy || id === null || !itemsById.has(id) || selected.has(id)) return;
    selected.add(id);
    renderTransfer();
  }

  function removeItem(rawId) {
    const id = uiItemId(rawId);
    if (busy || id === null || !selected.has(id)) return;
    selected.delete(id);
    renderTransfer();
  }

  function toggleItem(rawId) {
    const id = uiItemId(rawId);
    if (busy || id === null || !itemsById.has(id)) return;
    if (selected.has(id)) selected.delete(id);
    else selected.add(id);
    renderTransfer();
  }

  async function submitFullChoice(choice) {
    if (busy || !currentChoiceId) return;
    const base = api.getDaemonBase(currentProjectId);
    if (!base) {
      showError("Daemon offline — reconnect before resolving this divergence.");
      return;
    }
    const choiceId = currentChoiceId;
    setBusy(true);
    try {
      const body = { choiceId, choice };
      if (choice === "disk") body.mode = "all";
      const result = await daemonJson(base, "/initial-choice", {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify(body),
      });
      if (result && result.ok === false) throw new Error(result.error || "choice rejected");
      if (choice === "studio") api.toast?.("Studio will overwrite the local synced tree");
      else if (choice === "disk") api.toast?.("All disk changes queued for Studio");
      else api.toast?.("Initial sync canceled");
      close();
    } catch (error) {
      setBusy(false);
      showError(`Failed: ${error.message}`);
    }
  }

  async function submitSelected() {
    if (busy || !currentChoiceId) return;
    if (!detailAccumulator?.complete || detailError) {
      showError("Wait for the individual path list, retry it, or move all disk changes.");
      return;
    }
    const ids = selectedTransferIds(currentItems, selected);
    if (ids.length === 0) {
      showError("Stage at least one disk change before confirming.");
      return;
    }
    if (ids.length === detailTotal) {
      await submitFullChoice("disk");
      return;
    }
    const base = api.getDaemonBase(currentProjectId);
    if (!base) {
      showError("Daemon offline — reconnect before resolving this divergence.");
      return;
    }

    const choiceId = currentChoiceId;
    const submissionId = newSubmissionId();
    const chunks = selectionIdChunks(ids);
    setBusy(true);
    try {
      let selectedCount = 0;
      for (let chunkIndex = 0; chunkIndex < chunks.length; chunkIndex += 1) {
        const body = {
          choiceId,
          submissionId,
          chunkIndex,
          finalChunk: chunkIndex === chunks.length - 1,
          ids: chunks[chunkIndex],
        };
        if (chunkIndex === 0) body.restart = true;
        $detailProgress.textContent = `Submitting selection… ${selectedCount} of ${ids.length}`;
        const result = await postSelectionChunk(base, body);
        selectedCount += body.ids.length;
        assertSelectionReceipt(result, body, selectedCount);
      }
      api.toast?.(`${ids.length} disk changes queued for Studio`);
      close();
    } catch (error) {
      await abortSelection(base, choiceId, submissionId);
      setBusy(false);
      renderDetailProgress();
      showError(`Failed: ${error.message}`);
    }
  }

  async function postSelectionChunk(base, body) {
    let lastError = null;
    for (let attempt = 0; attempt < 3; attempt += 1) {
      try {
        return await daemonJson(base, "/initial-choice/selection", {
          method: "POST",
          headers: { "content-type": "application/json" },
          body: JSON.stringify(body),
        });
      } catch (error) {
        lastError = error;
      }
    }
    throw lastError || new Error("selection chunk failed");
  }

  async function abortSelection(base, choiceId, submissionId) {
    try {
      await daemonJson(base, "/initial-choice/selection", {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ op: "abort", choiceId, submissionId }),
      });
    } catch {
      // A final chunk may already have committed, or the daemon may be
      // offline. A fresh chunk zero uses restart:true so reloads cannot remain
      // wedged behind this abandoned, uncommitted accumulator.
    }
  }

  overlay.addEventListener("click", (event) => {
    if (busy) return;
    const action = event.target.closest("[data-act]")?.dataset.act;
    if (action === "studio" || action === "cancel") void submitFullChoice(action);
    if (action === "disk") {
      if (detailTotal > 0) showStep("transfer");
      else void submitFullChoice("disk");
    }
    if (action === "back") showStep("choice");
    if (action === "done") void submitSelected();
    if (action === "all-disk") void submitFullChoice("disk");
    if (action === "retry-details") void loadDetails({ restart: true });
    if (action === "all") {
      const visible = visibleItems();
      const unstaged = visible.filter((item) => !selected.has(item.id));
      if (unstaged.length) unstaged.forEach((item) => selected.add(item.id));
      else visible.forEach((item) => selected.delete(item.id));
      renderTransfer();
      return;
    }
    const filter = event.target.closest("[data-filter]")?.dataset.filter;
    if (filter) {
      filterAction = filter;
      visibleCache = null;
      renderTransfer();
      return;
    }
    const remove = event.target.closest("[data-remove-id]")?.dataset.removeId;
    if (remove !== undefined) {
      removeItem(remove);
      return;
    }
    const toggle = event.target.closest("[data-toggle-id]")?.dataset.toggleId;
    if (toggle !== undefined) toggleItem(toggle);
  });

  overlay.addEventListener("keydown", (event) => {
    if (busy || (event.key !== "Enter" && event.key !== " ")) return;
    const row = event.target.closest?.("[data-toggle-id]");
    if (!row) return;
    event.preventDefault();
    toggleItem(row.dataset.toggleId);
    // Re-render replaced the row node; keep keyboard position on its successor.
    overlay.querySelector(`[data-toggle-id="${cssEscape(row.dataset.toggleId)}"]`)?.focus();
  });

  $search.addEventListener("input", () => {
    searchText = $search.value;
    visibleCache = null;
    renderTransfer();
  });
  $search.addEventListener("keydown", (event) => {
    if (event.key === "Escape" && $search.value) {
      event.stopPropagation();
      $search.value = "";
      searchText = "";
      visibleCache = null;
      renderTransfer();
    }
  });
  $sort.addEventListener("change", () => {
    sortMode = $sort.value;
    visibleCache = null;
    renderTransfer();
  });

  // Pointer-based drag. HTML5 drag events never reach the page inside the
  // desktop app — Tauri's native drag-drop interception (kept on for the
  // projects view's folder drops) swallows them — so rows are dragged with
  // pointer capture instead, which behaves identically in both hosts.
  let drag = null;

  function beginPointerDrag(event, row, fromList) {
    if (busy || drag || event.button !== 0) return;
    const rawId = fromList === "disk" ? row.dataset.dragId : row.dataset.unstageId;
    const id = uiItemId(rawId);
    if (id === null || (fromList === "disk" && selected.has(id))) return;
    drag = {
      id,
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
    const row = event.target.closest("[data-drag-id]");
    if (row) beginPointerDrag(event, row, "disk");
  });
  $selectedList.addEventListener("pointerdown", (event) => {
    if (event.target.closest("button")) return;
    const row = event.target.closest("[data-unstage-id]");
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
    const { active, over, id, fromList } = drag;
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
      if (fromList === "disk") addItem(id);
      else removeItem(id);
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
    else void submitFullChoice("cancel");
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

function renderOverviewCounts(counts) {
  const groups = [
    { title: "Only on disk", hint: "missing in Studio", mark: "+", cls: "is-new", count: counts.create || 0 },
    { title: "Differs", hint: "changed in Studio or on disk", mark: "~", cls: "is-changed", count: counts.overwrite || 0 },
    { title: "Missing on disk", hint: "only in Studio", mark: "−", cls: "is-removed", count: counts.remove || 0 },
  ];
  return groups
    .filter((group) => group.count > 0)
    .map((group) => `
      <section class="initial-summary-group">
        <div class="initial-summary-label">
          <span><i class="${group.cls}" aria-hidden="true">${group.mark}</i> ${escape(group.title)}</span>
          <span>${group.count} · ${escape(group.hint)}</span>
        </div>
      </section>`)
    .join("");
}

function renderAvailableEmpty({
  detailReady,
  detailLoading,
  detailError,
  detailTotal,
  loaded,
}) {
  if (detailError) {
    return `<div class="transfer-empty is-quiet"><strong>Individual list unavailable</strong><small>Retry the list, or use all of Disk without loading paths.</small></div>`;
  }
  if (detailLoading || (!detailReady && detailTotal > 0)) {
    return `<div class="transfer-empty is-quiet"><strong>Loading disk changes</strong><small>${loaded} of ${detailTotal} paths received.</small></div>`;
  }
  return `<div class="transfer-empty is-quiet"><strong>No disk changes</strong><small>Studio and disk agree on every synced path.</small></div>`;
}

function assertSelectionReceipt(result, body, selectedCount) {
  if (!result || result.ok !== true) {
    throw new Error(result?.error || "selection chunk rejected");
  }
  if (result.choiceId !== body.choiceId || result.submissionId !== body.submissionId) {
    throw new Error("selection receipt belongs to another submission");
  }
  if (Number(result.acceptedChunk) !== body.chunkIndex) {
    throw new Error("selection receipt acknowledged the wrong chunk");
  }
  if (Number(result.nextChunk) !== body.chunkIndex + 1) {
    throw new Error("selection receipt has an invalid next chunk");
  }
  if (Number(result.selectedCount) !== selectedCount) {
    throw new Error("selection receipt has an invalid selected count");
  }
  if (body.finalChunk && result.committed !== true) {
    throw new Error("final selection chunk was not committed");
  }
  if (!body.finalChunk && result.committed === true) {
    throw new Error("selection committed before its final chunk");
  }
}

function newSubmissionId() {
  if (globalThis.crypto?.randomUUID) return globalThis.crypto.randomUUID();
  return `ui-${Date.now().toString(36)}-${Math.random().toString(36).slice(2, 14)}`;
}

function uiItemId(value) {
  const id = Number(value);
  return Number.isSafeInteger(id) && id >= 0 ? id : null;
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

function renderListOverflow(hidden, label, hint) {
  if (!hidden) return "";
  return `<div class="transfer-empty is-quiet transfer-overflow">
    <strong>+${hidden} ${escape(label)}</strong>
    <small>${escape(hint)}</small>
  </div>`;
}

function renderPathSpan(path, className) {
  const { parent, name } = splitDisplayPath(path);
  return `<span class="${className}" title="${escape(path)}">${parent ? `<span class="path-parent">${escape(parent)}/</span>` : ""}<span class="path-name">${escape(name)}</span></span>`;
}

function renderDiskItem(item, staged) {
  const edited = formatRelativeEdited(item.editedAt);
  return `
    <article class="transfer-file action-${item.action}${staged ? " is-staged" : ""}"
      data-drag-id="${item.id}"
      data-toggle-id="${item.id}" role="button" tabindex="0" aria-pressed="${staged}"
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
    <article class="transfer-file is-selected action-${item.action}" data-unstage-id="${item.id}">
      <span class="transfer-file-mark" aria-hidden="true">${markFor(item.action)}</span>
      <span class="transfer-file-copy">
        <span class="transfer-file-top">${renderPathSpan(item.path, "transfer-file-path")}</span>
        <small>${escape(transferVerb(item))}</small>
      </span>
      <button type="button" data-remove-id="${item.id}" aria-label="Unstage ${escape(item.path)}">✕</button>
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
