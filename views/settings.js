// views/settings.js — install helper, daemon info, controls.

import {
  PLATFORM, IS_WINDOWS,
  BINARY_REL, WIDGET_DIR_SHELL,
  PLUGIN_DIR_DISPLAY,
  parseBuildOutput,
  joinShell,
} from "../platform.js";
import { daemonJson } from "../bridge.js";
import { copyText, joinProjectFile, platformLabel } from "./runtime.js";

const RELEASES_URL = "https://github.com/Pugbread/ro-sync/releases";
const RUSTUP_URL   = "https://rustup.rs";

const DEFAULT_PORT = 7878;
const EXPECTED_PLUGIN_PROTOCOL = 2;
const PLUGIN_SOURCE_REL = "plugin/Plugin.luau";

async function writeFileViaExec(api, absPath, text) {
  return api.host.writeTextFile(absPath, text);
}

async function readFileViaExec(api, absPath) {
  return api.host.readTextFile(absPath);
}

export function mountSettings(root, api) {
  const desktop = api.host.isDesktop;
  root.innerHTML = `
    <section class="section">
      <h3>Secrets</h3>
      <p id="secret-storage-hint" style="color:var(--muted)">${desktop
        ? "Stored in the system credential vault when available, with a private mode-0600 Ro Sync credential file for CLI access or fallback; never in <code>ro-sync.json</code>."
        : "Stored in Terminal 64 widget state, not in <code>ro-sync.json</code>."} Use this for upload/API credentials.</p>
      <div class="secret-grid">
        <label>Roblox Open Cloud API key
          <div class="secret-input-row">
            <input id="secret-roblox-key" type="password" placeholder="Paste key or token" spellcheck="false" autocomplete="off" />
            <button id="secret-toggle" type="button">Show</button>
          </div>
        </label>
      </div>
      <div class="row" style="margin-top:8px">
        <button id="secret-save" class="primary">Save secrets</button>
        <button id="secret-clear" class="danger">Clear Roblox key</button>
      </div>
      <p id="secret-msg" style="color:var(--muted); margin-top:8px"></p>
    </section>

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
      <p>Install the Rojo-built <code>Plugin.rbxm</code> into Roblox Studio's Plugins folder. Studio loads it automatically.</p>
      <div class="row">
        <button id="set-install" class="primary">Install to Plugins folder</button>
        <button id="set-copy">Copy fallback Plugin.luau</button>
        <button id="set-open-folder">Open Plugins folder</button>
      </div>
      <p id="set-plugin-msg" style="color:var(--muted); margin-top:8px"></p>
    </section>

    <section class="section" id="sec-build">
      <h3>${desktop ? "Managed runtime" : "Daemon and lint compiler"}</h3>
      <p id="build-status" style="color:var(--warn)">Checking…</p>
      <pre id="build-log" class="build-log" hidden
        style="max-height:240px; overflow:auto; padding:8px 10px; background:var(--surface); border:1px solid var(--border); border-radius:4px; font-size:12px; white-space:pre-wrap; word-break:break-all;"></pre>
      <div class="row">
        <button id="build-run" class="primary" hidden>Build now</button>
        <a id="build-releases" href="${RELEASES_URL}" target="_blank" rel="noopener noreferrer">
          <button type="button">${desktop ? "View releases" : "Download release bundle"}</button>
        </a>
        <a id="build-rustup" href="${RUSTUP_URL}" target="_blank" rel="noopener noreferrer" hidden>
          <button type="button">Install Rust</button>
        </a>
      </div>
      <p style="color:var(--muted); margin-top:8px">
        ${desktop
          ? `The desktop app keeps its daemon, lint compiler, and Studio plugin on the same release. Runtime files live under <code id="build-dir-hint">—</code>.`
          : `Use <b>Build now</b> when the daemon is missing, or <b>Rebuild now</b>
            after updating Ro Sync. The build also attempts to install the pinned
            Luau compiler used by deep lint checks. Alternatively, extract your
            platform's pre-built release bundle at
            <code id="build-dir-hint">—</code>; it contains both tools in their
            expected folders.`}
      </p>
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
        <dt>protocol</dt><dd id="set-protocol">—</dd>
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
      <p style="color:var(--muted)">Host: <span id="set-host">${desktop ? "Desktop app" : "Terminal 64 widget"}</span></p>
      <p style="color:var(--muted)">Platform: <span id="set-platform">—</span></p>
    </section>
  `;

  const $bin = root.querySelector("#set-bin");
  const $port = root.querySelector("#set-port");
  const $pid = root.querySelector("#set-pid");
  const $base = root.querySelector("#set-base");
  const $protocol = root.querySelector("#set-protocol");
  const $copy = root.querySelector("#set-copy");
  const $install = root.querySelector("#set-install");
  const $openFolder = root.querySelector("#set-open-folder");
  const $pluginMsg = root.querySelector("#set-plugin-msg");
  const $start = root.querySelector("#set-start");
  const $stop = root.querySelector("#set-stop");
  const $restart = root.querySelector("#set-restart");

  const $secretRobloxKey = root.querySelector("#secret-roblox-key");
  const $secretToggle = root.querySelector("#secret-toggle");
  const $secretSave = root.querySelector("#secret-save");
  const $secretClear = root.querySelector("#secret-clear");
  const $secretMsg = root.querySelector("#secret-msg");

  const $buildSection = root.querySelector("#sec-build");
  const $buildStatus  = root.querySelector("#build-status");
  const $buildLog     = root.querySelector("#build-log");
  const $buildRun     = root.querySelector("#build-run");
  const $buildReleases = root.querySelector("#build-releases");
  const $buildRustup  = root.querySelector("#build-rustup");
  const $buildDirHint = root.querySelector("#build-dir-hint");

  const $ppSection = root.querySelector("#sec-project");
  const $ppName = root.querySelector("#pp-name");
  const $ppPriority = root.querySelector("#pp-priority");
  const $ppReconnect = root.querySelector("#pp-reconnect");
  const $ppPrompts = root.querySelector("#pp-prompts");
  const $ppSave = root.querySelector("#pp-save");
  const $ppReset = root.querySelector("#pp-reset");
  const $ppMsg = root.querySelector("#pp-msg");
  if (desktop) $buildReleases.hidden = true;

  // --- Binary / build management ---
  let runtimeInfo = null;

  async function loadRuntimeInfo() {
    try {
      runtimeInfo = await api.host.appInfo();
    } catch {
      runtimeInfo = null;
    }
    return runtimeInfo;
  }

  async function readDaemonHello() {
    const base = api.getDaemonBase();
    if (!base) return null;
    try {
      return await daemonJson(base, "/hello");
    } catch {
      return null;
    }
  }

  let buildRunning = false;
  async function refreshBuildBanner() {
    if (buildRunning) return;
    await loadRuntimeInfo();
    $buildDirHint.textContent = desktop
      ? (runtimeInfo?.resourceDir || runtimeInfo?.dataDir || "the Ro Sync app bundle")
      : WIDGET_DIR_SHELL;
    $buildSection.hidden = false;
    $buildLog.hidden = true;
    $buildLog.textContent = "";
    const [binary, hello] = await Promise.all([
      api.host.binaryStatus().catch(() => ({ present: false, cargo: false })),
      readDaemonHello(),
    ]);
    const have = !!binary.present;
    const hasCargo = !!binary.cargo;
    const protocol = hello && hello.pluginProtocol != null
      ? Number(hello.pluginProtocol)
      : Number.NaN;
    $protocol.textContent = Number.isFinite(protocol) ? String(protocol) : (hello ? "legacy / missing" : "—");
    $buildRun.textContent = have ? "Rebuild now" : "Build now";
    $buildRun.hidden = desktop || !hasCargo;
    $buildRustup.hidden = desktop || hasCargo;

    if (desktop && hello && protocol !== EXPECTED_PLUGIN_PROTOCOL) {
      $buildStatus.textContent =
        `The managed runtime is ready, but Studio is using protocol ${Number.isFinite(protocol) ? protocol : "legacy / missing"}. ` +
        `Reinstall the Studio plugin below and restart Studio.`;
    } else if (desktop && hello) {
      $buildStatus.textContent = `Managed runtime connected · protocol ${protocol}`;
    } else if (desktop) {
      $buildStatus.textContent = "Managed runtime is bundled with Ro Sync and starts when a project is served.";
    } else if (!have && hasCargo) {
      $buildStatus.textContent = "Daemon binary not found. Click “Build now” to compile it (~1-2 minutes).";
    } else if (!have) {
      $buildStatus.textContent =
        "Daemon binary not found, and the Rust toolchain (cargo) isn’t on PATH. " +
        "Install Rust first, or download a pre-built binary from Releases.";
    } else if (hello && protocol !== EXPECTED_PLUGIN_PROTOCOL) {
      $buildStatus.textContent =
        `Running daemon protocol ${Number.isFinite(protocol) ? protocol : "legacy / missing"}; ` +
        `this plugin requires protocol ${EXPECTED_PLUGIN_PROTOCOL}. Rebuild and restart the daemon.`;
    } else if (hello) {
      $buildStatus.textContent =
        `Daemon protocol ${protocol} is compatible. Rebuild after updating local Ro Sync source.`;
    } else {
      $buildStatus.textContent =
        "Daemon binary found but no running daemon answered. Rebuild after updating local Ro Sync source.";
    }
  }

  async function runBuild() {
    buildRunning = true;
    $buildRun.disabled = true;
    $buildStatus.textContent = "Stopping the daemon and rebuilding… this usually takes 1-2 minutes.";
    $buildLog.hidden = false;
    $buildLog.textContent = "$ " + (IS_WINDOWS ? "daemon\\build.ps1" : "daemon/build.sh") + "\n";
    let built = false;
    try {
      // Keep the active project/port target across the rebuild so a daemon
      // that was assigned a fallback port does not restart on 7878 and strand
      // the connected Studio plugin.
      const stopped = await api.killDaemon({ preserveTarget: true });
      if (!stopped) {
        $buildStatus.textContent = "Rebuild cancelled because the running daemon could not be stopped safely.";
        return;
      }
      // cargo build --release can take 1-2 minutes (cold cache). Override the
      // bridge's default 30s timeout so the promise doesn't reject early.
      const res = await api.host.buildDaemon({ timeoutMs: 5 * 60_000 });
      const out = (res && typeof res.stdout === "string" ? res.stdout : "")
        + (res && typeof res.stderr === "string" && res.stderr ? "\n" + res.stderr : "");
      const { ok, code, log } = parseBuildOutput(out);
      $buildLog.textContent = (log || "(no build output)").slice(-8000);
      $buildLog.scrollTop = $buildLog.scrollHeight;
      if (ok) {
        $buildStatus.textContent = "Build succeeded. Starting daemon…";
        api.toast("Daemon built");
        built = true;
      } else {
        $buildStatus.textContent = `Build failed (exit ${code ?? "?"}). See log below.`;
      }
    } catch (e) {
      $buildStatus.textContent = `Build failed or timed out: ${e.message}`;
    } finally {
      buildRunning = false;
      $buildRun.disabled = false;
      if (built) {
        await api.ensureDaemon();
        refresh();
        await refreshBuildBanner();
      }
    }
  }

  $buildRun.addEventListener("click", runBuild);

  let savedRobloxKey = "";
  let secretInputDirty = false;
  $secretSave.disabled = true;
  $secretClear.disabled = true;

  async function hydrateSecrets() {
    try {
      savedRobloxKey = String(await api.host.secretGet("robloxCloudApiKey") || "");
    } catch {
      savedRobloxKey = "";
    }
    if (!secretInputDirty) $secretRobloxKey.value = savedRobloxKey;
    $secretMsg.textContent = savedRobloxKey ? "Roblox key saved." : "No Roblox key saved.";
    $secretSave.disabled = false;
    $secretClear.disabled = false;
  }

  async function persistSecrets() {
    $secretSave.disabled = true;
    try {
      const value = $secretRobloxKey.value.trim();
      if (value) await api.host.secretSet("robloxCloudApiKey", value);
      else await api.host.secretDelete("robloxCloudApiKey");
      savedRobloxKey = value;
      secretInputDirty = false;
      $secretMsg.textContent = value ? "Roblox key saved." : "Roblox key cleared.";
      api.toast(value ? "Secrets saved" : "Secret cleared");
    } catch (e) {
      $secretMsg.textContent = `Save failed: ${e.message}`;
      api.toast(`Secrets save failed: ${e.message}`);
    } finally {
      $secretSave.disabled = false;
    }
  }

  async function clearRobloxKey() {
    $secretRobloxKey.value = "";
    await persistSecrets();
  }

  $secretToggle.addEventListener("click", () => {
    const showing = $secretRobloxKey.type === "text";
    $secretRobloxKey.type = showing ? "password" : "text";
    $secretToggle.textContent = showing ? "Show" : "Hide";
  });
  $secretRobloxKey.addEventListener("input", () => {
    secretInputDirty = true;
  });
  $secretSave.addEventListener("click", persistSecrets);
  $secretClear.addEventListener("click", clearRobloxKey);

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
    $bin.textContent = desktop
      ? (runtimeInfo?.daemonPath || "Bundled with desktop app")
      : joinShell(WIDGET_DIR_SHELL, BINARY_REL);
    const $plat = root.querySelector("#set-platform");
    if ($plat) $plat.textContent = platformLabel(runtimeInfo?.platform || PLATFORM);
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
    const cfgPath = joinProjectFile(proj.path, "ro-sync.json");
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
    const text = await api.host.readResourceText(PLUGIN_SOURCE_REL);
    if (!text) throw new Error("empty plugin source");
    return text;
  }

  $copy.addEventListener("click", async () => {
    try {
      const text = await readPluginFile();
      await copyText(api, text);
      $pluginMsg.textContent = `Copied ${text.length} chars to clipboard.`;
      api.toast("Plugin.luau copied");
    } catch (e) {
      $pluginMsg.textContent = `Copy failed: ${e.message}`;
    }
  });

  $install.addEventListener("click", async () => {
    try {
      const result = await api.host.pluginInstall();
      api.toast("Installed");
      const hello = await readDaemonHello();
      const protocol = hello && hello.pluginProtocol != null
        ? Number(hello.pluginProtocol)
        : Number.NaN;
      const daemonHint = hello && protocol !== EXPECTED_PLUGIN_PROTOCOL
        ? (desktop
          ? ` Studio protocol ${Number.isFinite(protocol) ? protocol : "legacy / missing"} is incompatible; restart Studio after installing.`
          : ` Daemon protocol ${Number.isFinite(protocol) ? protocol : "legacy / missing"} is incompatible; rebuild it below.`)
        : "";
      const installPath = result?.path || joinShell(PLUGIN_DIR_DISPLAY, "RoSync.rbxm");
      $pluginMsg.textContent =
        `Installed to ${installPath} — restart Studio.${daemonHint}`;
      await refreshBuildBanner();
    } catch (e) {
      $pluginMsg.textContent = `Install failed: ${e.message}`;
    }
  });

  $openFolder.addEventListener("click", async () => {
    try {
      await loadRuntimeInfo();
      const pluginDir = runtimeInfo?.pluginDir
        || parentPath(runtimeInfo?.pluginPath)
        || PLUGIN_DIR_DISPLAY;
      await api.host.openPath(pluginDir);
    } catch (e) {
      api.toast(`open failed: ${e.message}`);
    }
  });

  $start.addEventListener("click", async () => {
    const s = api.getState();
    if (!s.activeProjectId) {
      const target = (s.projects || []).find((p) => p.path === s.daemonProject);
      if (!target) {
        api.toast("Select a project in Projects before starting the daemon");
        return;
      }
      api.setState({ activeProjectId: target.id });
    }
    await api.ensureDaemon();
    refresh();
  });
  $stop.addEventListener("click", async () => {
    const s = api.getState();
    const active = (s.projects || []).find((p) => p.id === s.activeProjectId);
    api.setState({
      activeProjectId: null,
      daemonProject: active?.path || s.daemonProject || null,
    });
    await api.killDaemon({ preserveTarget: true });
    refresh();
  });
  $restart.addEventListener("click", async () => {
    const stopped = await api.killDaemon({ preserveTarget: true });
    if (!stopped) return;
    await api.ensureDaemon();
    refresh();
  });

  const offState = api.onBus("state", refresh);
  const offUp = api.onBus("daemon:up", () => { refresh(); refreshBuildBanner(); });
  const offDown = api.onBus("daemon:down", () => { refresh(); refreshBuildBanner(); });

  refresh();
  refreshBuildBanner().then(refresh);
  hydrateSecrets();

  return () => { offState(); offUp(); offDown(); };
}

function parentPath(path) {
  const value = String(path || "").replace(/[\\/]+$/, "");
  const index = Math.max(value.lastIndexOf("/"), value.lastIndexOf("\\"));
  return index > 0 ? value.slice(0, index) : "";
}
