import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import {
  APPEARANCE_OWNED_PROPERTIES,
  appearanceThemeOptions,
  applyAppearanceTheme,
  defaultAppearanceTheme,
  normalizeAppearanceTheme,
  resolveAppearanceTheme,
  sanitizeHostTheme,
} from "../views/theme.js";

assert.equal(defaultAppearanceTheme({ isDesktop: true, supportsHost: false }), "dark");
assert.equal(defaultAppearanceTheme({ isDesktop: false, supportsHost: true }), "host");
assert.equal(defaultAppearanceTheme({ isDesktop: false, supportsHost: false }), "system");

for (const value of ["system", "dark", "black", "light"]) {
  assert.equal(normalizeAppearanceTheme(value, { supportsHost: false }), value);
}
assert.equal(normalizeAppearanceTheme("host", { supportsHost: true }), "host");
assert.equal(
  normalizeAppearanceTheme("host", { supportsHost: false, isDesktop: true }),
  "dark",
  "unsupported hosts must never persist the Host-only option",
);
assert.equal(normalizeAppearanceTheme("midnight", { supportsHost: true }), "host");
assert.equal(normalizeAppearanceTheme({ id: "dark" }, { supportsHost: true }), "host");

assert.deepEqual(
  appearanceThemeOptions({ supportsHost: false }).map((option) => option.id),
  ["system", "dark", "black", "light"],
);
assert.deepEqual(
  appearanceThemeOptions({ supportsHost: true }).map((option) => option.id),
  ["system", "dark", "black", "light", "host"],
);

const hostileHostTheme = sanitizeHostTheme({
  background: "#010203",
  accent: "rgb(1, 2, 3)",
  "--surface": "#111111",
  "--made-up-token": "#ffffff",
  fg: "red; background: url(https://example.invalid)",
  border: "var(--secret)",
  Source: "SECRET_SOURCE",
});
assert.deepEqual(hostileHostTheme, {
  "--bg": "#010203",
  "--accent": "rgb(1, 2, 3)",
  "--surface": "#111111",
});
assert.doesNotMatch(JSON.stringify(hostileHostTheme), /SECRET|url|var\(/i);

const explicitDark = resolveAppearanceTheme("dark", {
  supportsHost: true,
  hostTheme: { background: "#ffffff", accent: "#ff0000" },
  systemDark: false,
});
assert.equal(explicitDark.preference, "dark");
assert.equal(explicitDark.tokens["--bg"], "#181818");
assert.equal(explicitDark.tokens["--accent"], "#7aa2d6");

const systemLight = resolveAppearanceTheme("system", { systemDark: false });
assert.equal(systemLight.effective, "light");
assert.equal(systemLight.colorScheme, "light");

const host = resolveAppearanceTheme("host", {
  supportsHost: true,
  hostTheme: {
    bg: "#101010",
    bgSecondary: "#181818",
    bgTertiary: "#202020",
    fg: "#eeeeee",
    fgMuted: "#aaaaaa",
    accentHover: "#77aaff",
  },
});
assert.equal(host.effective, "host");
assert.equal(host.tokens["--bg"], "#101010");
assert.equal(host.tokens["--fg"], "#eeeeee");
assert.equal(host.tokens["--surface"], "#181818");
assert.equal(host.tokens["--surface-2"], "#202020");
assert.equal(host.tokens["--surface-3"], "#202020");
assert.equal(host.tokens["--muted"], "#aaaaaa");
assert.equal(host.tokens["--accent-hover"], "#77aaff");
assert.equal(host.colorScheme, "dark");
assert.equal(resolveAppearanceTheme("host", {
  supportsHost: true,
  hostTheme: { bg: "white" },
}).colorScheme, "light");

class FakeStyle {
  constructor() {
    this.values = new Map();
    this.removed = [];
    this.colorScheme = "";
  }
  setProperty(name, value) { this.values.set(name, value); }
  removeProperty(name) {
    this.removed.push(name);
    this.values.delete(name);
  }
}

const root = { style: new FakeStyle(), dataset: {} };
root.style.setProperty("--bg", "hotpink");
root.style.setProperty("--surface-3", "lime");
const applied = applyAppearanceTheme(root, "light", { supportsHost: false });
assert.equal(applied.effective, "light");
assert.equal(root.dataset.theme, "light");
assert.equal(root.dataset.themePreference, "light");
assert.equal(root.style.colorScheme, "light");
assert.equal(root.style.values.get("--bg"), "#f3f6fa");
assert.ok(root.style.removed.includes("--bg"));
assert.ok(root.style.removed.includes("--surface-3"));
assert.ok(APPEARANCE_OWNED_PROPERTIES.length >= 20);

const pendingRoot = {
  style: new FakeStyle(),
  dataset: {},
  classList: {
    removed: [],
    remove(value) { this.removed.push(value); },
  },
};
applyAppearanceTheme(pendingRoot, "dark", { reveal: false });
assert.deepEqual(pendingRoot.classList.removed, []);
applyAppearanceTheme(pendingRoot, "dark", { reveal: true });
assert.deepEqual(pendingRoot.classList.removed, ["theme-pending"]);

const [appSource, settingsSource, styleSource] = await Promise.all([
  readFile(new URL("../app.js", import.meta.url), "utf8"),
  readFile(new URL("../views/settings.js", import.meta.url), "utf8"),
  readFile(new URL("../style.css", import.meta.url), "utf8"),
]);

assert.match(appSource, /appearanceTheme:\s*DEFAULT_APPEARANCE_THEME/);
assert.match(appSource, /normalizeAppearanceTheme\(/);
assert.match(appSource, /payload\?\.theme\?\.ui/);
assert.match(appSource, /onT64\("t64:state", applyHostThemePayload\)/);
const bootSource = appSource.slice(appSource.indexOf("(async function boot()"));
assert.ok(
  bootSource.indexOf("appearanceStateLoaded = true") < bootSource.indexOf("applyCurrentAppearanceTheme()"),
  "persisted state must be known before the appearance gate can reveal",
);
assert.ok(
  bootSource.indexOf("applyCurrentAppearanceTheme()") < bootSource.indexOf("navigate("),
  "the restored appearance must be applied before the first view mounts",
);
assert.ok(
  bootSource.indexOf("host.ready()") < bootSource.indexOf("scheduleAppearanceRevealFallback()"),
  "Host mode should wait for Terminal 64 before using the bounded fallback reveal",
);
assert.match(settingsSource, /<fieldset class="theme-picker"/);
assert.match(settingsSource, /type="radio" name="appearance-theme"/);
assert.match(settingsSource, /is-selected/);
assert.match(settingsSource, /supportsHost:\s*api\.host\.supports\.hostTheme/);
assert.doesNotMatch(styleSource, /transition\s*:\s*all(?:\s|;)/i);
assert.match(styleSource, /min-height:\s*108px/);
assert.match(styleSource, /html\.theme-pending body/);
assert.match(styleSource, /color:\s*var\(--accent-contrast\)/);
assert.match(styleSource, /background:\s*var\(--code-bg\)/);

function relativeLuminance(hex) {
  const channels = hex.match(/[\da-f]{2}/gi).map((part) => Number.parseInt(part, 16) / 255);
  const linear = channels.map((channel) => (
    channel <= 0.04045 ? channel / 12.92 : ((channel + 0.055) / 1.055) ** 2.4
  ));
  return 0.2126 * linear[0] + 0.7152 * linear[1] + 0.0722 * linear[2];
}
function contrast(first, second) {
  const a = relativeLuminance(first);
  const b = relativeLuminance(second);
  return (Math.max(a, b) + 0.05) / (Math.min(a, b) + 0.05);
}
assert.ok(contrast("5f6c7f", "edf2f7") >= 4.5, "light muted text must meet WCAG AA");
assert.ok(contrast("78879b", "e3eaf3") >= 3, "light control boundaries must meet 3:1");

console.log("theme policy checks passed");
