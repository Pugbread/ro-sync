// views/preview.js — threshold change-preview modal.
//
// Mount once at app boot. Subscribes to the "batch-preview" bus event fanned
// out by app.js. Shape:
//   { summary: { added, updated, removed }, ops: [...], source?: "daemon"|"heuristic" }
//
// On Accept, POSTs {accept:true} to <daemonBase>/batch-decision (ignoring 404s
// so this is harmless until the daemon implements the endpoint).
// On Reject, POSTs {accept:false}.
//
// Counts are color-coded (green/yellow/red) mirroring Argon's processor/mod.rs:184.

export function mountPreviewModal(api) {
  // api: { onBus, getDaemonBase, getState, toast }
  const overlay = document.createElement("div");
  overlay.className = "modal-overlay preview-overlay";
  overlay.hidden = true;
  overlay.setAttribute("role", "dialog");
  overlay.setAttribute("aria-modal", "true");
  overlay.setAttribute("aria-labelledby", "pv-title");
  overlay.innerHTML = `
    <div class="modal-card preview-card" role="document">
      <h2 class="modal-title" id="pv-title">Review pending changes</h2>
      <p class="modal-sub" data-sub>A large batch is about to be applied.</p>
      <div class="preview-counts">
        <div class="pv-count pv-add"><span class="n" data-f="added">0</span><span class="l">added</span></div>
        <div class="pv-count pv-upd"><span class="n" data-f="updated">0</span><span class="l">updated</span></div>
        <div class="pv-count pv-rem"><span class="n" data-f="removed">0</span><span class="l">removed</span></div>
      </div>
      <ul class="preview-list" data-list></ul>
      <div class="modal-actions">
        <button class="primary" data-act="accept">Accept</button>
        <button data-act="reject">Reject</button>
      </div>
      <p class="modal-err" data-err hidden></p>
    </div>
  `;
  document.body.appendChild(overlay);

  const $sub = overlay.querySelector("[data-sub]");
  const $added = overlay.querySelector('[data-f="added"]');
  const $updated = overlay.querySelector('[data-f="updated"]');
  const $removed = overlay.querySelector('[data-f="removed"]');
  const $list = overlay.querySelector("[data-list]");
  const $err = overlay.querySelector("[data-err]");
  const $card = overlay.querySelector(".preview-card");
  const buttons = overlay.querySelectorAll("[data-act]");

  let busy = false;
  let currentOps = null;

  function open(data) {
    const sum = (data && data.summary) || {};
    const a = numOr(sum.added), u = numOr(sum.updated), r = numOr(sum.removed);
    $added.textContent = String(a);
    $updated.textContent = String(u);
    $removed.textContent = String(r);
    $sub.textContent =
      data && data.source === "heuristic"
        ? `${a + u + r} ops arrived rapidly — confirm before applying.`
        : `The daemon wants to apply ${a + u + r} ops.`;
    $list.innerHTML = "";
    const ops = Array.isArray(data && data.ops) ? data.ops : [];
    currentOps = ops;
    for (const op of ops.slice(0, 30)) {
      const li = document.createElement("li");
      li.className = opClass(op);
      li.textContent = opLabel(op);
      $list.appendChild(li);
    }
    if (ops.length > 30) {
      const li = document.createElement("li");
      li.className = "pv-more";
      li.textContent = `… and ${ops.length - 30} more`;
      $list.appendChild(li);
    }
    $err.hidden = true;
    $err.textContent = "";
    setBusy(false);
    overlay.hidden = false;
    const first = overlay.querySelector('[data-act="accept"]');
    if (first) first.focus();
  }

  function close() {
    overlay.hidden = true;
    currentOps = null;
    setBusy(false);
  }

  function setBusy(b) {
    busy = b;
    buttons.forEach((btn) => { btn.disabled = b; });
    $card.classList.toggle("is-busy", b);
  }

  async function submit(accept) {
    if (busy) return;
    const base = api.getDaemonBase();
    setBusy(true);
    if (!base) {
      // No daemon — nothing to POST to. Treat as local-only decision.
      api.toast && api.toast(accept ? "Accepted (local)" : "Rejected (local)");
      close();
      return;
    }
    try {
      const url = base.replace(/\/+$/, "") + "/batch-decision";
      const res = await fetch(url, {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ accept }),
      });
      // 404 is expected until daemon lands the endpoint — swallow.
      if (!res.ok && res.status !== 404) {
        throw new Error(`status ${res.status}`);
      }
      api.toast && api.toast(accept ? "Applied" : "Rejected");
      close();
    } catch (e) {
      setBusy(false);
      $err.hidden = false;
      $err.textContent = `Failed: ${e.message}`;
    }
  }

  for (const btn of buttons) {
    btn.addEventListener("click", () => submit(btn.dataset.act === "accept"));
  }
  overlay.addEventListener("keydown", (e) => {
    if (e.key === "Escape" && !busy) submit(false);
  });

  api.onBus("batch-preview", (data) => {
    if (!data || typeof data !== "object") return;
    // Honor DisplayPrompts on the active project — if off, auto-accept.
    const s = api.getState && api.getState();
    const proj = s && (s.projects || []).find((p) => p.id === s.activeProjectId);
    const cfg = (proj && proj.settings) || {};
    if (cfg.DisplayPrompts === "off") {
      // Silent auto-accept.
      const base = api.getDaemonBase();
      if (base) {
        const url = base.replace(/\/+$/, "") + "/batch-decision";
        fetch(url, {
          method: "POST",
          headers: { "content-type": "application/json" },
          body: JSON.stringify({ accept: true }),
        }).catch(() => {});
      }
      return;
    }
    open(data);
  });
}

function numOr(n) {
  return typeof n === "number" && Number.isFinite(n) ? n : 0;
}

function opClass(op) {
  const k = String((op && (op.op || op.kind || op.action)) || "").toLowerCase();
  if (k.includes("add") || k.includes("create")) return "pv-line pv-add";
  if (k.includes("remove") || k.includes("delete")) return "pv-line pv-rem";
  return "pv-line pv-upd";
}

function opLabel(op) {
  if (!op) return "";
  if (typeof op === "string") return op;
  const k = op.op || op.kind || op.action || "op";
  const p = op.path || op.file || op.id || op.name || "";
  return p ? `${k}  ${p}` : String(k);
}
