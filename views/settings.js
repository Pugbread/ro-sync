// views/settings.js — install helper, daemon info, controls.

const DEFAULT_PORT = 7878;
const PLUGIN_FOLDER_DISPLAY = "~/Documents/Roblox/Plugins";
const PLUGIN_FOLDER_SHELL = "$HOME/Documents/Roblox/Plugins";
const WIDGET_DIR_SHELL = "$HOME/.terminal64/widgets/ro-sync";
const BINARY_REL = "daemon/rosync-darwin-arm64";
const PLUGIN_REL = "plugin/Plugin.luau";

// POSIX single-quote shell escape.
function shQuote(s) {
  return "'" + String(s).replace(/'/g, "'\\''") + "'";
}

// Write arbitrary text to a path via `echo <b64> | base64 -d > path`.
// Avoids quoting headaches for JSON payloads containing newlines/quotes.
async function writeFileViaExec(api, absPath, text) {
  const b64 = btoa(unescape(encodeURIComponent(text)));
  const command = `echo ${shQuote(b64)} | base64 -d > ${shQuote(absPath)}`;
  return api.t64("t64:exec", { command });
}

async function readFileViaExec(api, absPath) {
  const command = `cat ${shQuote(absPath)} 2>/dev/null || true`;
  const res = await api.t64("t64:exec", { command });
  return (res && typeof res.stdout === "string") ? res.stdout : "";
}

export function mountSettings(root, api) {
  root.innerHTML = `
    <section class="section" id="sec-project" hidden>
      <h3>Project: <span id="pp-name">—</span></h3>
      <p style="color:var(--muted)">Stored in <code>ro-sync.json</code> at the project root. The daemon hot-reloads the file.</p>
      <div class="pp-grid">
        <label>Initial sync priority
          <select id="pp-priority">
            <option value="Prompt">Prompt (ask me)</option>
            <option value="ServerPrefer">Server prefer (Studio wins)</option>
            <option value="FilesystemPrefer">Filesystem prefer (disk wins)</option>
          </select>
        </label>
        <label>Auto reconnect
          <select id="pp-reconnect">
            <option value="on">on</option>
            <option value="off">off</option>
          </select>
        </label>
        <label>Display prompts
          <select id="pp-prompts">
            <option value="on">on</option>
            <option value="off">off</option>
          </select>
        </label>
      </div>
      <div class="row" style="margin-top:8px">
        <button id="pp-save" class="primary">Save</button>
        <button id="pp-reset">Reset</button>
      </div>
      <p id="pp-msg" style="color:var(--muted); margin-top:8px"></p>
    </section>

    <section class="section">
      <h3>Studio plugin</h3>
      <p>Copy <code>Plugin.luau</code> into Roblox Studio's Plugins folder. Studio loads it automatically.</p>
      <div class="row">
        <button id="set-copy" class="primary">Copy Plugin.luau</button>
        <button id="set-install">Install to Plugins folder</button>
        <button id="set-open-folder">Open Plugins folder</button>
      </div>
      <p id="set-plugin-msg" style="color:var(--muted); margin-top:8px"></p>
    </section>

    <section class="section">
      <h3>Daemon</h3>
      <dl class="kv">
        <dt>binary</dt><dd id="set-bin">—</dd>
        <dt>default port</dt><dd>${DEFAULT_PORT}</dd>
        <dt>port</dt><dd id="set-port">—</dd>
        <dt>project</dt><dd id="set-project-path">—</dd>
        <dt>pid</dt><dd id="set-pid">—</dd>
        <dt>base url</dt><dd id="set-base">—</dd>
      </dl>
      <div class="row" style="margin-top:8px">
        <button id="set-start">Start</button>
        <button id="set-stop" class="danger">Stop</button>
        <button id="set-restart">Restart</button>
      </div>
    </section>

    <section class="section">
      <h3>About</h3>
      <p>Ro Sync — zero-config two-way sync between Roblox Studio and your filesystem.</p>
      <p style="color:var(--muted)">macOS arm64 only in v1.</p>
    </section>
  `;

  const $bin = root.querySelector("#set-bin");
  const $port = root.querySelector("#set-port");
  const $pid = root.querySelector("#set-pid");
  const $base = root.querySelector("#set-base");
  const $copy = root.querySelector("#set-copy");
  const $install = root.querySelector("#set-install");
  const $openFolder = root.querySelector("#set-open-folder");
  const $pluginMsg = root.querySelector("#set-plugin-msg");
  const $start = root.querySelector("#set-start");
  const $stop = root.querySelector("#set-stop");
  const $restart = root.querySelector("#set-restart");

  const $ppSection = root.querySelector("#sec-project");
  const $ppName = root.querySelector("#pp-name");
  const $ppPriority = root.querySelector("#pp-priority");
  const $ppReconnect = root.querySelector("#pp-reconnect");
  const $ppPrompts = root.querySelector("#pp-prompts");
  const $ppSave = root.querySelector("#pp-save");
  const $ppReset = root.querySelector("#pp-reset");
  const $ppMsg = root.querySelector("#pp-msg");

  function activeProject() {
    const s = api.getState();
    return (s.projects || []).find((p) => p.id === s.activeProjectId) || null;
  }

  function defaultProjectSettings() {
    return { InitialSyncPriority: "Prompt", AutoReconnect: "on", DisplayPrompts: "on" };
  }

  function refresh() {
    const s = api.getState();
    const base = api.getDaemonBase();
    $bin.textContent = WIDGET_DIR_SHELL + "/" + BINARY_REL;
    $port.textContent = s.daemonPort ?? DEFAULT_PORT;
    $pid.textContent = s.daemonPid ?? "—";
    $base.textContent = base || "—";
    const $proj = root.querySelector("#set-project-path");
    if ($proj) $proj.textContent = s.daemonProject || "—";

    const proj = activeProject();
    if (!proj) {
      $ppSection.hidden = true;
      return;
    }
    $ppSection.hidden = false;
    $ppName.textContent = proj.name || proj.path || "project";
    const cfg = { ...defaultProjectSettings(), ...(proj.settings || {}) };
    $ppPriority.value = cfg.InitialSyncPriority;
    $ppReconnect.value = cfg.AutoReconnect;
    $ppPrompts.value = cfg.DisplayPrompts;
  }

  async function saveProjectSettings() {
    const proj = activeProject();
    if (!proj) return;
    const cfg = {
      InitialSyncPriority: $ppPriority.value,
      AutoReconnect: $ppReconnect.value,
      DisplayPrompts: $ppPrompts.value,
    };
    // Persist on the project record.
    const s = api.getState();
    const next = (s.projects || []).map((p) =>
      p.id === proj.id ? { ...p, settings: cfg } : p
    );
    api.setState({ projects: next });

    // Merge into ro-sync.json at the project root (read → merge → write).
    const cfgPath = proj.path.replace(/\/+$/, "") + "/ro-sync.json";
    let existing = {};
    try {
      const raw = await readFileViaExec(api, cfgPath);
      if (raw && raw.trim()) existing = JSON.parse(raw);
    } catch (e) {
      // Missing or malformed — start fresh.
      existing = {};
    }
    const merged = { ...existing, ...cfg };
    try {
      await writeFileViaExec(api, cfgPath, JSON.stringify(merged, null, 2) + "\n");
      $ppMsg.textContent = `Saved to ${cfgPath}`;
      api.toast("Project settings saved");
    } catch (e) {
      $ppMsg.textContent = `Write failed: ${e.message}`;
      api.toast(`ro-sync.json write failed: ${e.message}`);
    }
  }

  function resetProjectSettings() {
    const proj = activeProject();
    if (!proj) return;
    const cfg = { ...defaultProjectSettings(), ...(proj.settings || {}) };
    $ppPriority.value = cfg.InitialSyncPriority;
    $ppReconnect.value = cfg.AutoReconnect;
    $ppPrompts.value = cfg.DisplayPrompts;
    $ppMsg.textContent = "";
  }

  $ppSave.addEventListener("click", saveProjectSettings);
  $ppReset.addEventListener("click", resetProjectSettings);

  async function readPluginFile() {
    try {
      const res = await api.t64("t64:read-file", { path: "{widgetDir}/" + PLUGIN_REL });
      const text = (res && (res.content || res.text || res.data)) ?? (typeof res === "string" ? res : null);
      if (!text) throw new Error("empty");
      return text;
    } catch (e) {
      // Fallback: fetch relative to the widget's own origin.
      try {
        const r = await fetch(PLUGIN_REL);
        if (r.ok) return await r.text();
      } catch {}
      throw e;
    }
  }

  $copy.addEventListener("click", async () => {
    try {
      const text = await readPluginFile();
      await navigator.clipboard.writeText(text);
      $pluginMsg.textContent = `Copied ${text.length} chars to clipboard.`;
      api.toast("Plugin.luau copied");
    } catch (e) {
      // Fallback via host.
      try {
        await api.t64("t64:clipboard-write", { path: "{widgetDir}/" + PLUGIN_REL });
        api.toast("Plugin.luau copied");
        $pluginMsg.textContent = "Copied via host.";
      } catch (e2) {
        $pluginMsg.textContent = `Copy failed: ${e.message}`;
      }
    }
  });

  $install.addEventListener("click", async () => {
    const command = `mkdir -p ${PLUGIN_FOLDER_SHELL} && cp ${WIDGET_DIR_SHELL}/${PLUGIN_REL} ${PLUGIN_FOLDER_SHELL}/RoSync.lua && rm -f ${PLUGIN_FOLDER_SHELL}/RoSync.luau`;
    try {
      const res = await api.t64("t64:exec", { command });
      if (res && res.code !== 0 && res.code != null) {
        throw new Error(res.stderr?.trim() || `exit ${res.code}`);
      }
      api.toast("Installed");
      $pluginMsg.textContent = `Installed to ${PLUGIN_FOLDER_DISPLAY}/RoSync.lua — restart Studio`;
    } catch (e) {
      $pluginMsg.textContent = `Install failed: ${e.message}`;
    }
  });

  $openFolder.addEventListener("click", async () => {
    const command = `mkdir -p ${PLUGIN_FOLDER_SHELL} && /usr/bin/open ${PLUGIN_FOLDER_SHELL}`;
    try {
      const res = await api.t64("t64:exec", { command });
      if (res && res.code !== 0 && res.code != null) {
        throw new Error(res.stderr?.trim() || `exit ${res.code}`);
      }
    } catch (e) {
      api.toast(`open failed: ${e.message}`);
    }
  });

  $start.addEventListener("click", async () => {
    await api.ensureDaemon();
    refresh();
  });
  $stop.addEventListener("click", async () => {
    await api.killDaemon();
    refresh();
  });
  $restart.addEventListener("click", async () => {
    await api.killDaemon();
    await api.ensureDaemon();
    refresh();
  });

  const offState = api.onBus("state", refresh);
  const offUp = api.onBus("daemon:up", refresh);
  const offDown = api.onBus("daemon:down", refresh);

  refresh();

  return () => { offState(); offUp(); offDown(); };
}
