// views/theme.js — persisted appearance presets and safe host-theme adaptation.

export const APPEARANCE_THEME_IDS = Object.freeze([
  "system",
  "dark",
  "black",
  "light",
  "host",
]);

const APPEARANCE_THEME_SET = new Set(APPEARANCE_THEME_IDS);

export const APPEARANCE_THEME_OPTIONS = Object.freeze([
  Object.freeze({
    id: "system",
    label: "System",
    description: "Follow this computer's light or dark appearance.",
    preview: "system",
  }),
  Object.freeze({
    id: "dark",
    label: "Dark",
    description: "Calm graphite surfaces with restrained contrast.",
    preview: "dark",
  }),
  Object.freeze({
    id: "black",
    label: "Black / OLED",
    description: "OLED-friendly surfaces with true black depth.",
    preview: "black",
  }),
  Object.freeze({
    id: "light",
    label: "Light",
    description: "Bright neutral surfaces with crisp contrast.",
    preview: "light",
  }),
  Object.freeze({
    id: "host",
    label: "Host",
    description: "Match the colors supplied by Terminal 64.",
    preview: "host",
  }),
]);

const COMMON_DARK = Object.freeze({
  "--fg": "#d8d8d8",
  "--muted": "#969696",
  "--accent": "#7aa2d6",
  "--accent-hover": "#91b3df",
  "--accent-contrast": "#111820",
  "--danger": "#e17982",
  "--danger-contrast": "#1d1113",
  "--danger-soft": "rgba(225, 121, 130, 0.09)",
  "--ok": "#79c49c",
  "--warn": "#c8a967",
  "--warn-contrast": "#20190b",
  "--code-bg": "#000000",
  "--diff-add-bg": "rgba(121, 196, 156, 0.2)",
  "--diff-add-fg": "#c9ead9",
  "--diff-remove-bg": "rgba(225, 121, 130, 0.2)",
  "--diff-remove-fg": "#f2c7cb",
  "--modal-scrim": "rgba(0, 0, 0, 0.62)",
  "--scroll-thumb-hover": "#535353",
  "--surface-highlight": "#ffffff",
  "--brand-filter": "none",
  "--shadow-1": "0 1px 2px rgba(0, 0, 0, 0.24)",
  "--shadow-2": "0 4px 14px rgba(0, 0, 0, 0.28)",
  "--shadow-modal": "0 0 0 1px rgba(255, 255, 255, 0.08), 0 18px 54px rgba(0, 0, 0, 0.58)",
  "--shadow-border": "0 0 0 1px rgba(255, 255, 255, 0.075)",
  "--shadow-border-hover": "0 0 0 1px rgba(255, 255, 255, 0.14)",
});

export const APPEARANCE_THEME_TOKENS = Object.freeze({
  dark: Object.freeze({
    ...COMMON_DARK,
    "--bg": "#181818",
    "--border": "#343434",
    "--surface": "#1e1e1e",
    "--surface-2": "#252525",
    "--surface-3": "#2d2d2d",
  }),
  black: Object.freeze({
    ...COMMON_DARK,
    "--bg": "#000000",
    "--fg": "#dedede",
    "--muted": "#929292",
    "--border": "#292929",
    "--surface": "#0a0a0a",
    "--surface-2": "#121212",
    "--surface-3": "#1b1b1b",
    "--shadow-1": "0 1px 2px rgba(0, 0, 0, 0.55)",
    "--shadow-2": "0 3px 14px rgba(0, 0, 0, 0.7)",
    "--shadow-modal": "0 1px 0 rgba(255, 255, 255, 0.025) inset, 0 2px 8px rgba(0, 0, 0, 0.7), 0 24px 64px rgba(0, 0, 0, 0.86)",
  }),
  light: Object.freeze({
    "--bg": "#f3f6fa",
    "--fg": "#17202d",
    "--muted": "#5f6c7f",
    "--accent": "#2563eb",
    "--accent-hover": "#1d4ed8",
    "--accent-contrast": "#ffffff",
    "--border": "#78879b",
    "--surface": "#ffffff",
    "--surface-2": "#edf2f7",
    "--surface-3": "#e3eaf3",
    "--danger": "#c93748",
    "--danger-contrast": "#ffffff",
    "--danger-soft": "rgba(201, 55, 72, 0.09)",
    "--ok": "#187d50",
    "--warn": "#9a6217",
    "--warn-contrast": "#ffffff",
    "--code-bg": "#f8fafc",
    "--diff-add-bg": "rgba(24, 125, 80, 0.13)",
    "--diff-add-fg": "#105c3a",
    "--diff-remove-bg": "rgba(201, 55, 72, 0.12)",
    "--diff-remove-fg": "#942635",
    "--modal-scrim": "rgba(26, 36, 51, 0.32)",
    "--scroll-thumb-hover": "#aab5c4",
    "--surface-highlight": "#000000",
    "--brand-filter": "brightness(0) saturate(100%)",
    "--shadow-1": "0 1px 2px rgba(26, 36, 51, 0.08), 0 1px 3px rgba(26, 36, 51, 0.06)",
    "--shadow-2": "0 3px 8px rgba(26, 36, 51, 0.11), 0 10px 28px rgba(26, 36, 51, 0.08)",
    "--shadow-modal": "0 1px 0 rgba(255, 255, 255, 0.8) inset, 0 3px 10px rgba(26, 36, 51, 0.14), 0 24px 64px rgba(26, 36, 51, 0.2)",
    "--shadow-border": "0 0 0 1px rgba(26, 36, 51, 0.1)",
    "--shadow-border-hover": "0 0 0 1px rgba(26, 36, 51, 0.19)",
  }),
});

export const APPEARANCE_OWNED_PROPERTIES = Object.freeze(
  [...new Set(Object.values(APPEARANCE_THEME_TOKENS).flatMap((tokens) => Object.keys(tokens)))],
);

const HOST_COLOR_MAP = Object.freeze({
  bg: ["--bg"],
  background: ["--bg"],
  bgSecondary: ["--surface"],
  bgTertiary: ["--surface-2", "--surface-3"],
  fg: ["--fg"],
  foreground: ["--fg"],
  fgSecondary: ["--muted"],
  fgMuted: ["--muted"],
  muted: ["--muted"],
  accent: ["--accent"],
  accentHover: ["--accent-hover"],
  "accent-hover": ["--accent-hover"],
  border: ["--border"],
  surface: ["--surface"],
  surface2: ["--surface-2"],
  surface3: ["--surface-3"],
  danger: ["--danger"],
  warn: ["--warn"],
  ok: ["--ok"],
  "--bg": ["--bg"],
  "--fg": ["--fg"],
  "--muted": ["--muted"],
  "--accent": ["--accent"],
  "--accent-hover": ["--accent-hover"],
  "--border": ["--border"],
  "--surface": ["--surface"],
  "--surface-2": ["--surface-2"],
  "--surface-3": ["--surface-3"],
  "--danger": ["--danger"],
  "--warn": ["--warn"],
  "--ok": ["--ok"],
});

function safeHostColor(value) {
  if (typeof value !== "string") return null;
  const color = value.trim();
  if (!color || color.length > 128 || /[;{}]|url\s*\(|var\s*\(/i.test(color)) return null;
  if (globalThis.CSS?.supports) return CSS.supports("color", color) ? color : null;
  return /^(?:#[\da-f]{3,8}|(?:rgb|rgba|hsl|hsla|oklab|oklch)\([^)]{1,96}\)|transparent|black|white)$/i.test(color)
    ? color
    : null;
}

export function sanitizeHostTheme(theme) {
  if (!theme || typeof theme !== "object" || Array.isArray(theme)) return {};
  const tokens = {};
  for (const [key, value] of Object.entries(theme)) {
    const properties = HOST_COLOR_MAP[key];
    const color = properties ? safeHostColor(value) : null;
    if (!color) continue;
    for (const property of properties) tokens[property] = color;
  }
  return tokens;
}

function directColorChannels(background) {
  if (typeof background !== "string") return null;
  const value = background.trim();
  if (/^white$/i.test(value)) return [255, 255, 255];
  if (/^black$/i.test(value)) return [0, 0, 0];
  const hex = value.match(/^#([\da-f]{3}|[\da-f]{6}|[\da-f]{8})$/i)?.[1];
  let channels = null;
  if (hex?.length === 3) {
    channels = [...hex].map((channel) => Number.parseInt(channel + channel, 16));
  } else if (hex?.length === 6 || hex?.length === 8) {
    channels = [0, 2, 4].map((offset) => Number.parseInt(hex.slice(offset, offset + 2), 16));
  } else {
    const rgb = value.match(/^rgba?\(\s*(\d+(?:\.\d+)?)\D+(\d+(?:\.\d+)?)\D+(\d+(?:\.\d+)?)/i);
    if (rgb) channels = rgb.slice(1, 4).map(Number);
  }
  return channels?.every((channel) => Number.isFinite(channel)) ? channels : null;
}

function hostColorScheme(background) {
  let channels = directColorChannels(background);
  // CSS.supports accepts named, HSL, OKLCH, and other valid colors. Ask the
  // browser to normalize those formats before choosing native control colors.
  if (!channels && globalThis.document?.createElement && globalThis.getComputedStyle) {
    const probe = document.createElement("span");
    probe.style.color = String(background || "");
    probe.hidden = true;
    document.documentElement.appendChild(probe);
    channels = directColorChannels(getComputedStyle(probe).color);
    probe.remove();
  }
  if (!channels) return "dark";
  const [red, green, blue] = channels.map((channel) => Math.max(0, Math.min(255, channel)) / 255);
  return (0.2126 * red + 0.7152 * green + 0.0722 * blue) > 0.62 ? "light" : "dark";
}

export function defaultAppearanceTheme({ supportsHost = false, isDesktop = false } = {}) {
  if (isDesktop) return "dark";
  return supportsHost ? "host" : "system";
}

export function normalizeAppearanceTheme(value, options = {}) {
  const fallback = defaultAppearanceTheme(options);
  if (typeof value !== "string" || !APPEARANCE_THEME_SET.has(value)) return fallback;
  if (value === "host" && !options.supportsHost) return fallback;
  return value;
}

export function appearanceThemeOptions({ supportsHost = false } = {}) {
  return APPEARANCE_THEME_OPTIONS.filter((option) => option.id !== "host" || supportsHost);
}

export function resolveAppearanceTheme(value, {
  supportsHost = false,
  isDesktop = false,
  hostTheme = null,
  systemDark = true,
} = {}) {
  const preference = normalizeAppearanceTheme(value, { supportsHost, isDesktop });
  if (preference === "system") {
    const effective = systemDark ? "dark" : "light";
    return {
      preference,
      effective,
      colorScheme: systemDark ? "dark" : "light",
      tokens: { ...APPEARANCE_THEME_TOKENS[effective] },
    };
  }
  if (preference === "host") {
    const hostTokens = sanitizeHostTheme(hostTheme);
    const colorScheme = hostColorScheme(hostTokens["--bg"]);
    return {
      preference,
      effective: "host",
      colorScheme,
      tokens: {
        ...APPEARANCE_THEME_TOKENS[colorScheme === "light" ? "light" : "dark"],
        ...hostTokens,
      },
    };
  }
  return {
    preference,
    effective: preference,
    colorScheme: preference === "light" ? "light" : "dark",
    tokens: { ...APPEARANCE_THEME_TOKENS[preference] },
  };
}

export function applyAppearanceTheme(root, value, options = {}) {
  if (!root?.style || !root?.dataset) return resolveAppearanceTheme(value, options);
  const resolved = resolveAppearanceTheme(value, options);
  for (const property of APPEARANCE_OWNED_PROPERTIES) root.style.removeProperty(property);
  for (const [property, token] of Object.entries(resolved.tokens)) {
    root.style.setProperty(property, token);
  }
  root.style.colorScheme = resolved.colorScheme;
  root.dataset.theme = resolved.effective;
  root.dataset.themePreference = resolved.preference;
  if (options.reveal !== false) root.classList?.remove("theme-pending");
  return resolved;
}
