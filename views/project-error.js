// views/project-error.js — turn noisy daemon startup output into a concise,
// actionable project state while preserving a deduplicated raw diagnostic.

export function collapseDaemonDiagnostic(raw) {
  const text = String(raw || "")
    .replace(/\u001b\[[0-9;]*m/g, "")
    .replace(/\s+(?=rosync listening on http:\/\/)/g, "\n")
    .replace(/\s+(?=Error:\s+(?:serve|daemon start):)/g, "\n")
    .trim();
  if (!text) return "No diagnostic was returned.";

  const seen = new Set();
  const lines = [];
  for (const rawLine of text.split(/\r?\n/)) {
    const line = rawLine.trim();
    if (!line || seen.has(line)) continue;
    seen.add(line);
    lines.push(line);
  }
  return lines.join("\n");
}

export function formatProjectFailure(raw, projectPath = "") {
  const diagnostic = collapseDaemonDiagnostic(raw);
  const legacyLeafMatch = diagnostic.match(
    /legacy leaf script (.+?) uses the reserved init-marker filename grammar;\s*rename it to (.+?) before syncing/i,
  );
  if (legacyLeafMatch) {
    const sourcePath = legacyLeafMatch[1].trim();
    const canonicalPath = legacyLeafMatch[2].trim();
    return {
      code: "legacy-init-leaf",
      statusLabel: "Rename required",
      title: "This script filename needs an escaped spelling",
      summary: "Its literal name overlaps the syntax Ro Sync reserves for a parent script source.",
      guidance: `Rename ${basename(sourcePath)} to ${basename(canonicalPath)}, then retry. The script name in Studio will stay the same.`,
      path: resolveDiagnosticDirectory(projectPath, sourcePath),
      files: [basename(sourcePath), basename(canonicalPath)],
      sourcePath,
      canonicalPath,
      diagnostic,
    };
  }

  const markerMatch = diagnostic.match(
    /multiple init source markers in (.*?):\s*"([^"]+)" and "([^"]+)"/i,
  );
  if (markerMatch) {
    const path = markerMatch[1].trim();
    const first = markerMatch[2];
    const second = markerMatch[3];
    const files = [first, second];
    const deletePaths = conflictDeletePaths(projectPath, path, files);
    return {
      code: "multiple-init-markers",
      statusLabel: "File conflict",
      title: "Fix these files or delete them?",
      summary: "Both files claim the same parent script, so Ro Sync stopped serving.",
      guidance: deletePaths.length === files.length
        ? "Fix keeps both files and opens their folder. Delete permanently removes both files, then retries the daemon."
        : "Fix keeps both files and opens their folder. Delete is unavailable because the reported paths could not be verified.",
      path,
      files,
      deletePaths,
      diagnostic,
    };
  }

  if (/address already in use|port\s+\d+.*(?:busy|in use)/i.test(diagnostic)) {
    return {
      code: "port-unavailable",
      statusLabel: "Port unavailable",
      title: "The daemon port is already in use",
      summary: "Another process is occupying the port Ro Sync tried to use.",
      guidance: "Stop the conflicting process or retry after it releases the port.",
      path: projectPath,
      files: [],
      diagnostic,
    };
  }

  if (/permission denied|operation not permitted|not authorized/i.test(diagnostic)) {
    const pathPermission = /(?:scan|metadata|canonicaliz|filesystem|file|directory|path|project).{0,120}(?:permission denied|operation not permitted|not authorized)|(?:permission denied|operation not permitted|not authorized).{0,120}(?:scan|metadata|canonicaliz|filesystem|file|directory|path|project|(?:\/|[A-Za-z]:\\)\S*)/i.test(diagnostic);
    return {
      code: "permission-denied",
      statusLabel: "Permission needed",
      title: pathPermission
        ? "Ro Sync cannot access part of this project"
        : "The operating system denied a daemon operation",
      summary: pathPermission
        ? "The daemon stopped before serving because a required path was not readable."
        : "Ro Sync was not allowed to complete one of its startup operations.",
      guidance: pathPermission
        ? "Check the path in Details, update its permissions, then retry."
        : "Open Details to identify the blocked operation, update the relevant system permission, then retry.",
      path: projectPath,
      files: [],
      diagnostic,
    };
  }

  if (/failed to fetch|networkerror|timed out|timeout|connection (?:refused|reset)/i.test(diagnostic)) {
    return {
      code: "daemon-unreachable",
      statusLabel: "Connection lost",
      title: "The daemon is not responding",
      summary: "Ro Sync could not reach the daemon currently assigned to this project.",
      guidance: "Retry the daemon. If it still fails, use Details to check the port and process error.",
      path: projectPath,
      files: [],
      diagnostic,
    };
  }

  return {
    code: "daemon-start-failed",
    statusLabel: "Start failed",
    title: "The daemon could not start",
    summary: "Ro Sync stopped before it could begin serving this project.",
    guidance: "Review the diagnostic below, fix the reported file or process issue, then retry.",
    path: projectPath,
    files: [],
    diagnostic,
  };
}

function basename(path) {
  const value = String(path || "").replace(/[\\/]+$/, "");
  const index = Math.max(value.lastIndexOf("/"), value.lastIndexOf("\\"));
  return index >= 0 ? value.slice(index + 1) : value;
}

function dirname(path) {
  const value = String(path || "").replace(/[\\/]+$/, "");
  const index = Math.max(value.lastIndexOf("/"), value.lastIndexOf("\\"));
  return index > 0 ? value.slice(0, index) : "";
}

function resolveDiagnosticDirectory(projectPath, sourcePath) {
  const directory = dirname(sourcePath);
  if (!directory) return projectPath;
  if (/^(?:\/|[A-Za-z]:[\\/]|\\\\)/.test(directory)) return directory;
  const root = String(projectPath || "").replace(/[\\/]+$/, "");
  if (!root) return directory;
  const separator = root.includes("\\") && !root.includes("/") ? "\\" : "/";
  return `${root}${separator}${directory.replace(/[\\/]+/g, separator)}`;
}

export function conflictDeletePaths(projectPath, directoryPath, files) {
  const root = normalizeAbsolutePath(projectPath);
  const directory = normalizeAbsolutePath(directoryPath);
  if (!root || !directory || !isPathInsideRoot(root, directory)) return [];
  if (!Array.isArray(files) || files.length < 1 || files.length > 8) return [];
  if (!files.every(isInitMarkerLeaf)) return [];
  const separator = String(directoryPath).includes("\\") && !String(directoryPath).includes("/")
    ? "\\"
    : "/";
  const base = String(directoryPath).replace(/[\\/]+$/, "");
  return files.map((file) => `${base}${separator}${file}`);
}

function normalizeAbsolutePath(path) {
  let value = String(path || "").trim().replace(/\\/g, "/").replace(/\/+$/, "");
  if (/^\/\/\?\/UNC\//i.test(value)) value = `//${value.slice("//?/UNC/".length)}`;
  else if (/^\/\/\?\//i.test(value)) value = value.slice("//?/".length);
  if (!/^(?:\/|[A-Za-z]:\/|\/\/[^/]+\/[^/]+)/.test(value)) return "";
  if (/(?:^|\/)\.\.?(?:\/|$)/.test(value) || /[\u0000-\u001f\u007f]/.test(value)) return "";
  value = value.replace(/^([a-z]):\//, (_, drive) => `${drive.toUpperCase()}:/`);
  return value;
}

function isPathInsideRoot(root, path) {
  if (path === root) return true;
  return path.startsWith(`${root}/`);
}

function isInitMarkerLeaf(file) {
  const value = String(file || "");
  if (!value || value.includes("/") || value.includes("\\") || /[\u0000-\u001f\u007f]/.test(value)) {
    return false;
  }
  const match = value.match(/^(.+?)(?:\.(server|client))?\.(lua|luau)$/i);
  if (!match) return false;
  const stem = match[1];
  return /^init$/i.test(stem) || /^init \(.+\)$/i.test(stem);
}
