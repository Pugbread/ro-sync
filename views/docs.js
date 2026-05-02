// views/docs.js — searchable Ro Sync command reference.

const DOCS_BUNDLE_REL = "docs/client-commands.generated.json";

export function mountDocs(root, api) {
  root.innerHTML = `
    <div class="docs-shell">
      <div class="page-header docs-header">
        <div class="page-titles">
          <h1 class="page-title">Command Docs</h1>
          <p class="page-sub" data-docs-summary>Loading command catalogue.</p>
        </div>
      </div>
      <div class="search-toolbar docs-toolbar">
        <div class="search-wrap">
          <svg viewBox="0 0 16 16" width="14" height="14" fill="none" stroke="currentColor" stroke-width="1.5" aria-hidden="true">
            <circle cx="7" cy="7" r="4.5"/>
            <path d="m10.5 10.5 3 3" stroke-linecap="round"/>
          </svg>
          <input id="docs-search" type="search" placeholder="Search commands" autocomplete="off" />
        </div>
        <div class="filter-pills docs-filters" role="tablist" aria-label="Command categories"></div>
      </div>
      <div class="docs-grid">
        <aside class="docs-index" aria-label="Commands"></aside>
        <section class="docs-results" aria-live="polite"></section>
      </div>
    </div>
  `;

  const $summary = root.querySelector("[data-docs-summary]");
  const $search = root.querySelector("#docs-search");
  const $filters = root.querySelector(".docs-filters");
  const $index = root.querySelector(".docs-index");
  const $results = root.querySelector(".docs-results");

  let commands = [];
  let categories = [];
  let selectedCategory = "All";

  function renderFilters() {
    const names = ["All", ...categories];
    $filters.innerHTML = names.map((name) => `
      <button class="pill docs-filter" type="button" role="tab" aria-selected="${name === selectedCategory ? "true" : "false"}" aria-pressed="${name === selectedCategory ? "true" : "false"}" data-category="${escapeAttr(name)}">
        ${escape(name)}
      </button>
    `).join("");
  }

  function filteredCommands() {
    const q = $search.value.trim().toLowerCase();
    return commands.filter((command) => {
      if (selectedCategory !== "All" && command.category !== selectedCategory) return false;
      if (!q) return true;
      const haystack = [
        command.title,
        command.category,
        command.description,
        command.usage,
        ...(command.examples || []),
        ...(command.notes || []),
      ].join(" ").toLowerCase();
      return haystack.includes(q);
    });
  }

  function render() {
    const visible = filteredCommands();
    $summary.textContent = `${visible.length} of ${commands.length} commands`;

    $index.innerHTML = visible.length ? visible.map((command) => `
      <button class="docs-index-item" type="button" data-slug="${escapeAttr(command.slug)}">
        <span class="docs-index-name">${escape(command.title)}</span>
        <span class="docs-index-category">${escape(command.category)}</span>
      </button>
    `).join("") : "";

    if (!visible.length) {
      $results.innerHTML = `<div class="empty docs-empty">No matching commands.</div>`;
      return;
    }

    $results.innerHTML = visible.map((command) => commandCard(command)).join("");
  }

  function commandCard(command) {
    const examples = (command.examples || []).length
      ? `<div class="docs-section">
          <h3>Examples</h3>
          ${codeBlock(command.examples.join("\n"), `${command.slug}-examples`, "Copy examples")}
        </div>`
      : "";
    const notes = (command.notes || []).length
      ? `<div class="docs-section">
          <h3>Notes</h3>
          <ul class="docs-notes">${command.notes.map((note) => `<li>${inlineCode(escape(note))}</li>`).join("")}</ul>
        </div>`
      : "";
    return `
      <article class="docs-command" id="docs-${escapeAttr(command.slug)}">
        <div class="docs-command-head">
          <div>
            <h2>${escape(command.title)}</h2>
            <p>${inlineCode(escape(command.description))}</p>
          </div>
          <span class="docs-category">${escape(command.category)}</span>
        </div>
        <div class="docs-section">
          <h3>Usage</h3>
          ${codeBlock(command.usage, `${command.slug}-usage`, "Copy usage")}
        </div>
        ${examples}
        ${notes}
      </article>
    `;
  }

  function codeBlock(text, id, label) {
    return `
      <div class="docs-code">
        <pre><code>${escape(text)}</code></pre>
        <button class="docs-copy" type="button" data-copy="${escapeAttr(encodeURIComponent(text))}" aria-label="${escapeAttr(label)}">Copy</button>
      </div>
    `;
  }

  $filters.addEventListener("click", (event) => {
    const button = event.target.closest("[data-category]");
    if (!button) return;
    selectedCategory = button.dataset.category || "All";
    renderFilters();
    render();
  });

  $index.addEventListener("click", (event) => {
    const button = event.target.closest("[data-slug]");
    if (!button) return;
    const el = root.querySelector(`#docs-${cssEscape(button.dataset.slug)}`);
    if (el) el.scrollIntoView({ block: "start", behavior: "smooth" });
  });

  $results.addEventListener("click", async (event) => {
    const button = event.target.closest("[data-copy]");
    if (!button) return;
    try {
      await navigator.clipboard.writeText(decodeURIComponent(button.dataset.copy || ""));
      api.toast("Copied command");
    } catch (e) {
      api.toast(`copy failed: ${e.message}`);
    }
  });

  $search.addEventListener("input", render);

  let cancelled = false;
  loadDocs(api).then((bundle) => {
    if (cancelled) return;
    commands = Array.isArray(bundle.commands) ? bundle.commands : [];
    categories = Array.isArray(bundle.categories) ? bundle.categories : [];
    renderFilters();
    render();
  }).catch((e) => {
    if (cancelled) return;
    $summary.textContent = "Command docs unavailable.";
    $results.innerHTML = `<div class="empty docs-empty">Could not load command docs: ${escape(e.message)}</div>`;
  });

  return () => { cancelled = true; };
}

async function loadDocs(api) {
  try {
    const res = await fetch(DOCS_BUNDLE_REL);
    if (res.ok) return await res.json();
  } catch {}

  const res = await api.t64("t64:read-file", { path: "{widgetDir}/" + DOCS_BUNDLE_REL });
  const text = (res && (res.content || res.text || res.data)) ?? (typeof res === "string" ? res : null);
  if (!text) throw new Error("empty docs bundle");
  return JSON.parse(text);
}

function escape(value) {
  return String(value ?? "")
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;")
    .replace(/'/g, "&#39;");
}

function escapeAttr(value) {
  return escape(value).replace(/`/g, "&#96;");
}

function inlineCode(html) {
  return html.replace(/`([^`]+)`/g, "<code>$1</code>");
}

function cssEscape(value) {
  if (window.CSS && typeof window.CSS.escape === "function") return window.CSS.escape(value);
  return String(value || "").replace(/[^a-zA-Z0-9_-]/g, "\\$&");
}
