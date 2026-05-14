// views/overwrite.js — blocking modal for "initial-choice-needed" daemon events.
//
// Mount once at app boot. It subscribes to two bus events (fanned out by
// app.js from the daemon WebSocket stream):
//   - initial-choice-needed : { choiceId, diskStats, studioStats, comparison? }
//   - initial-choice-made   : { choiceId, ... }  (dismiss if still showing)
// On button click it POSTs {choiceId, choice} to <daemonBase>/initial-choice.
import { installDocumentEscape } from "./runtime.js";

export function mountOverwriteModal(api) {
  // api is { onBus, getDaemonBase, toast }.
  const overlay = document.createElement("div");
  overlay.className = "modal-overlay";
  overlay.hidden = true;
  overlay.setAttribute("role", "dialog");
  overlay.setAttribute("aria-modal", "true");
  overlay.setAttribute("aria-labelledby", "ow-title");
  overlay.innerHTML = `
    <div class="modal-card initial-card" role="document">
      <div class="initial-hero">
        <div class="initial-icon" aria-hidden="true">RS</div>
        <div class="initial-copy">
          <h2 class="modal-title" id="ow-title">Choose source of truth</h2>
          <p class="modal-sub">Only paths managed by Ro Sync are compared. Pick which side should overwrite the synced tree.</p>
        </div>
      </div>
      <div class="initial-summary" data-summary hidden>
        <div class="initial-summary-head">
          <span>Synced path changes</span>
          <span data-summary-total>—</span>
        </div>
        <div class="initial-summary-groups" data-summary-groups></div>
      </div>
      <div class="initial-decision">
        <div class="initial-decision-copy">
          <strong>Resolve the initial sync</strong>
          <span>Keep Disk pushes local synced files to Studio. Keep Studio writes the Studio tree back to disk.</span>
        </div>
        <div class="modal-actions">
          <button class="primary" data-act="disk">Keep Disk</button>
          <button class="primary" data-act="studio">Keep Studio</button>
          <button data-act="cancel">Cancel</button>
        </div>
      </div>
      <p class="modal-err" data-err hidden></p>
    </div>
  `;
  document.body.appendChild(overlay);

  const $card = overlay.querySelector(".modal-card");
  const $summary = overlay.querySelector("[data-summary]");
  const $summaryTotal = overlay.querySelector("[data-summary-total]");
  const $summaryGroups = overlay.querySelector("[data-summary-groups]");
  const $err = overlay.querySelector("[data-err]");
  const buttons = overlay.querySelectorAll("[data-act]");

  let currentChoiceId = null;
  let busy = false;

  function open(data) {
    currentChoiceId = data.choiceId || null;
    renderComparison(data.comparison);
    $err.hidden = true;
    $err.textContent = "";
    setBusy(false);
    overlay.hidden = false;
    // Focus first action button for keyboard users.
    const firstBtn = overlay.querySelector('[data-act="disk"]');
    if (firstBtn) firstBtn.focus();
  }

  function close() {
    overlay.hidden = true;
    currentChoiceId = null;
    setBusy(false);
  }

  function setBusy(b) {
    busy = b;
    buttons.forEach((btn) => { btn.disabled = b; });
    $card.classList.toggle("is-busy", b);
  }

  function renderComparison(comparison) {
    const groups = comparisonGroups(comparison);
    const total = groups.reduce((sum, group) => sum + group.items.length, 0);
    if (!total) {
      $summary.hidden = false;
      $summaryTotal.textContent = "unavailable";
      $summaryGroups.innerHTML = `
        <div class="initial-summary-fallback">
          This daemon did not send a synced-path summary. Rebuild and restart
          the Ro Sync daemon to see New Files, Changed Files, and Removed Files.
        </div>
      `;
      return;
    }

    $summary.hidden = false;
    $summaryTotal.textContent = `${total} ${total === 1 ? "path" : "paths"}`;
    $summaryGroups.innerHTML = groups
      .filter((group) => group.items.length > 0)
      .map(renderComparisonGroup)
      .join("");
  }

  function comparisonGroups(comparison) {
    if (!comparison || typeof comparison !== "object") return [];
    return [
      {
        title: "New Files",
        hint: "on disk only",
        mark: "+",
        cls: "is-new",
        items: Array.isArray(comparison.newFiles) ? comparison.newFiles : [],
      },
      {
        title: "Changed Files",
        hint: "different source or class",
        mark: "~",
        cls: "is-changed",
        items: Array.isArray(comparison.changedFiles) ? comparison.changedFiles : [],
      },
      {
        title: "Removed Files",
        hint: "in Studio only",
        mark: "-",
        cls: "is-removed",
        items: Array.isArray(comparison.removedFiles) ? comparison.removedFiles : [],
      },
    ];
  }

  function renderComparisonGroup(group) {
    const limit = 8;
    const visible = group.items.slice(0, limit);
    const more = group.items.length - visible.length;
    return `
      <section class="initial-summary-group">
        <div class="initial-summary-label">
          <span>${escape(group.title)}</span>
          <span>${group.items.length} · ${escape(group.hint)}</span>
        </div>
        <ul>
          ${visible.map((item) => renderComparisonItem(group, item)).join("")}
          ${more > 0 ? `<li class="initial-summary-more">+${more} more</li>` : ""}
        </ul>
      </section>
    `;
  }

  function renderComparisonItem(group, item) {
    const path = item && item.path ? item.path : "(unknown)";
    const meta = comparisonItemMeta(group, item || {});
    return `
      <li class="${group.cls}">
        <span class="initial-summary-mark">${escape(group.mark)}</span>
        <span class="initial-summary-path">${escape(path)}</span>
        <span class="initial-summary-meta">${escape(meta)}</span>
      </li>
    `;
  }

  function comparisonItemMeta(group, item) {
    if (group.title === "Changed Files") {
      const reasons = [];
      if (item.sourceChanged) reasons.push("source");
      if (item.classChanged) reasons.push("class");
      return reasons.length ? reasons.join(", ") : "changed";
    }
    return item.class || item.kind || "";
  }

  async function submit(choice) {
    if (busy || !currentChoiceId) return;
    const base = api.getDaemonBase();
    if (!base) {
      $err.hidden = false;
      $err.textContent = "Daemon offline — cannot send choice.";
      return;
    }
    setBusy(true);
    try {
      const url = base.replace(/\/+$/, "") + "/initial-choice";
      const res = await fetch(url, {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ choiceId: currentChoiceId, choice }),
      });
      if (!res.ok) throw new Error(`status ${res.status}`);
      api.toast && api.toast(
        choice === "cancel" ? "Canceled" : `Keeping ${choice}`
      );
      close();
    } catch (e) {
      setBusy(false);
      $err.hidden = false;
      $err.textContent = `Failed: ${e.message}`;
    }
  }

  for (const btn of buttons) {
    btn.addEventListener("click", () => submit(btn.dataset.act));
  }

  // ESC treated as cancel.
  installDocumentEscape((e) => {
    if (!overlay.hidden && !busy) {
      e.preventDefault();
      submit("cancel");
    }
  });

  // Resolve the active project's per-project settings, if any.
  function activeSettings() {
    const s = api.getState && api.getState();
    if (!s) return {};
    const proj = (s.projects || []).find((p) => p.id === s.activeProjectId);
    return (proj && proj.settings) || {};
  }

  api.onBus("initial-choice-needed", (data) => {
    if (!data || typeof data !== "object") return;
    const cfg = activeSettings();
    const priority = cfg.InitialSyncPriority || "Prompt";
    // ServerPrefer = Studio wins (overwrite disk); FilesystemPrefer = Disk wins.
    if (priority === "ServerPrefer" || priority === "FilesystemPrefer") {
      currentChoiceId = data.choiceId || null;
      const choice = priority === "ServerPrefer" ? "studio" : "disk";
      submit(choice);
      return;
    }
    // DisplayPrompts: off also suppresses the modal — cancel silently.
    if (cfg.DisplayPrompts === "off") {
      currentChoiceId = data.choiceId || null;
      submit("cancel");
      return;
    }
    open(data);
  });
  api.onBus("initial-choice-made", (data) => {
    if (!data || typeof data !== "object") return;
    if (!currentChoiceId) return;
    // Close if this resolution is for the currently-shown prompt
    // (or if no choiceId is attached, assume it resolves the current one).
    if (!data.choiceId || data.choiceId === currentChoiceId) {
      api.toast && api.toast("Resolved elsewhere");
      close();
    }
  });
}

function escape(value) {
  return String(value ?? "")
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;")
    .replace(/'/g, "&#39;");
}
