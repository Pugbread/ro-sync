// Small browser/runtime helpers shared by widget views.

export async function copyText(api, text) {
  const value = String(text ?? "");
  let clipboardError = null;
  const nav = typeof navigator !== "undefined" ? navigator : null;
  if (nav && nav.clipboard && typeof nav.clipboard.writeText === "function") {
    try {
      await nav.clipboard.writeText(value);
      return;
    } catch (e) {
      clipboardError = e;
    }
  }

  if (api && typeof api.t64 === "function") {
    try {
      await api.t64("t64:clipboard-write", { text: value, timeoutMs: 5000 });
      return;
    } catch {}
  }

  throw clipboardError || new Error("clipboard API unavailable");
}

export function platformLabel(platform) {
  if (platform === "darwin") return "macOS";
  if (platform === "windows") return "Windows";
  if (platform === "linux") return "Linux";
  return String(platform || "Unknown");
}

export function installDocumentEscape(handler) {
  const onKeydown = (event) => {
    if (event.key === "Escape") handler(event);
  };
  document.addEventListener("keydown", onKeydown);
  return () => document.removeEventListener("keydown", onKeydown);
}

export function pathFromDrop(event) {
  const dt = event && event.dataTransfer;
  if (!dt) return "";

  const uriList = dt.getData && dt.getData("text/uri-list");
  const uriPath = firstPathFromUriList(uriList);
  if (uriPath) return uriPath;

  const text = dt.getData && dt.getData("text/plain");
  const textPath = firstPathFromText(text);
  if (textPath) return textPath;

  const files = Array.from(dt.files || []);
  for (const file of files) {
    const filePath = file && (file.path || file.webkitRelativePath);
    if (looksLikePath(filePath)) return stripTrailingSeparators(filePath);
  }
  return "";
}

function firstPathFromUriList(value) {
  if (!value) return "";
  const lines = String(value).split(/\r?\n/).map((line) => line.trim()).filter(Boolean);
  for (const line of lines) {
    if (line.startsWith("#")) continue;
    const parsed = pathFromFileUrl(line);
    if (parsed) return parsed;
  }
  return "";
}

function pathFromFileUrl(value) {
  try {
    const url = new URL(value);
    if (url.protocol !== "file:") return "";
    const decodedPath = decodeURIComponent(url.pathname || "");
    if (url.hostname && url.hostname !== "localhost") {
      return stripTrailingSeparators(`\\\\${url.hostname}${decodedPath.replace(/\//g, "\\")}`);
    }
    return stripTrailingSeparators(decodedPath.replace(/^\/([A-Za-z]:[\\/])/, "$1"));
  } catch {
    return "";
  }
}

function firstPathFromText(value) {
  if (!value) return "";
  const first = String(value).split(/\r?\n/).map((line) => line.trim()).find(Boolean);
  if (!first) return "";
  const withoutQuotes = first.replace(/^["']|["']$/g, "");
  const parsedUrl = pathFromFileUrl(withoutQuotes);
  if (parsedUrl) return parsedUrl;
  return looksLikePath(withoutQuotes) ? stripTrailingSeparators(withoutQuotes) : "";
}

function looksLikePath(value) {
  const s = String(value || "").trim();
  return /^(?:[A-Za-z]:[\\/]|[\\/]{1,2}|~)/.test(s);
}

function stripTrailingSeparators(value) {
  return String(value || "").replace(/[\\/]+$/, "");
}
