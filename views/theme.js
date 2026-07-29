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
    description: "Ro Sync's balanced blue-black default.",
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
  "--fg": "#f3f6fb",
  "--muted": "#929cab",
  "--accent": "#4f8ef7",
  "--accent-hover": "#71a6fa",
  "--accent-contrast": "#07111f",
  "--danger": "#ef6872",
  "--danger-contrast": "#161014",
  "--danger-soft": "rgba(239, 104, 114, 0.11)",
  "--ok": "#4bc58a",
  "--warn": "#dda64f",
  "--warn-contrast": "#211706",
  "--code-bg": "#000000",
  "--diff-add-bg": "rgba(102, 204, 102, 0.25)",
  "--diff-add-fg": "#ccffcc",
  "--diff-remove-bg": "rgba(255, 102, 102, 0.25)",
  "--diff-remove-fg": "#ffcccc",
  "--modal-scrim": "rgba(0, 0, 0, 0.55)",
  "--scroll-thumb-hover": "#3a4351",
  "--surface-highlight": "#ffffff",
  "--brand-filter": "none",
  "--shadow-1": "0 1px 1px rgba(0, 0, 0, 0.25), 0 1px 2px rgba(0, 0, 0, 0.15)",
  "--shadow-2": "0 2px 4px rgba(0, 0, 0, 0.3), 0 4px 12px rgba(0, 0, 0, 0.25)",
  "--shadow-modal": "0 1px 0 rgba(255, 255, 255, 0.03) inset, 0 2px 6px rgba(0, 0, 0, 0.35), 0 20px 56px rgba(0, 0, 0, 0.6)",
  "--shadow-border": "0 0 0 1px rgba(255, 255, 255, 0.075)",
  "--shadow-border-hover": "0 0 0 1px rgba(255, 255, 255, 0.14)",
});

export const APPEARANCE_THEME_TOKENS = Object.freeze({
  dark: Object.freeze({
    ...COMMON_DARK,
    "--bg": "#0e1116",
    "--border": "#292f3a",
    "--surface": "#151922",
    "--surface-2": "#1c222d",
    "--surface-3": "#242b37",
  }),
  black: Object.freeze({
    ...COMMON_DARK,
    "--bg": "#000000",
    "--fg": "#f6f8fc",
    "--muted": "#8e99aa",
    "--border": "#202630",
    "--surface": "#07090d",
    "--surface-2": "#0c1017",
    "--surface-3": "#141a23",
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
