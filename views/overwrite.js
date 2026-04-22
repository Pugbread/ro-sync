// views/overwrite.js — blocking modal for "initial-choice-needed" SSE events.
//
// Mount once at app boot. It subscribes to two bus events (fanned out by
// app.js from the daemon /events stream):
//   - initial-choice-needed : { choiceId, diskStats, studioStats }
//   - initial-choice-made   : { choiceId, ... }  (dismiss if still showing)
// On button click it POSTs {choiceId, choice} to <daemonBase>/initial-choice.

export function mountOverwriteModal(api) {
  // api is { onBus, getDaemonBase, toast }.
  const overlay = document.createElement("div");
  overlay.className = "modal-overlay";
  overlay.hidden = true;
  overlay.setAttribute("role", "dialog");
  overlay.setAttribute("aria-modal", "true");
  overlay.setAttribute("aria-labelledby", "ow-title");
  overlay.innerHTML = `
    <div class="modal-card" role="document">
      <h2 class="modal-title" id="ow-title">Initial sync — choose source of truth</h2>
      <p class="modal-sub">The daemon and the Studio plugin are both populated. Pick which side overwrites the other.</p>
      <div class="modal-compare">
        <div class="compare-card" data-side="disk">
          <div class="compare-label">Disk</div>
          <div class="compare-stat"><span class="n" data-f="diskScripts">—</span> scripts</div>
          <div class="compare-stat"><span class="n" data-f="diskInstances">—</span> instances</div>
        </div>
        <div class="compare-vs">vs</div>
        <div class="compare-card" data-side="studio">
          <div class="compare-label">Studio</div>
          <div class="compare-stat"><span class="n" data-f="studioScripts">—</span> scripts</div>
          <div class="compare-stat"><span class="n" data-f="studioInstances">—</span> instances</div>
        </div>
      </div>
      <div class="modal-actions">
        <button class="primary" data-act="disk">Keep Disk (overwrite Studio)</button>
        <button class="primary" data-act="studio">Keep Studio (overwrite Disk)</button>
        <button data-act="cancel">Cancel</button>
      </div>
      <p class="modal-err" data-err hidden></p>
    </div>
  `;
  document.body.appendChild(overlay);

  const $card = overlay.querySelector(".modal-card");
  const $diskScripts = overlay.querySelector('[data-f="diskScripts"]');
  const $diskInstances = overlay.querySelector('[data-f="diskInstances"]');
  const $studioScripts = overlay.querySelector('[data-f="studioScripts"]');
  const $studioInstances = overlay.querySelector('[data-f="studioInstances"]');
  const $err = overlay.querySelector("[data-err]");
  const buttons = overlay.querySelectorAll("[data-act]");

  let currentChoiceId = null;
  let busy = false;

  function open(data) {
    currentChoiceId = data.choiceId || null;
    const d = data.diskStats || {};
    const s = data.studioStats || {};
    $diskScripts.textContent = numOrDash(d.scriptCount);
    $diskInstances.textContent = numOrDash(d.instanceCount);
    $studioScripts.textContent = numOrDash(s.scriptCount);
    $studioInstances.textContent = numOrDash(s.instanceCount);
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
  overlay.addEventListener("keydown", (e) => {
    if (e.key === "Escape" && !busy) submit("cancel");
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

function numOrDash(n) {
  return typeof n === "number" && Number.isFinite(n) ? String(n) : "—";
}
