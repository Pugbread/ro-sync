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
      guidance: `Use Fix filename offline to rename ${basename(sourcePath)} to ${basename(canonicalPath)} safely. The source and script name in Studio will stay the same.`,
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
    const hasPlainInit = files.some((file) =>
      /^init(?:\.(?:server|client))?\.(?:luau|lua)$/.test(file)
    );
    return {
      code: "multiple-init-markers",
      statusLabel: "File conflict",
      title: "Two init files map to the same script",
      summary: "Ro Sync stopped before serving to avoid choosing the wrong source file.",
      guidance: hasPlainInit
        ? `Use Compare & resolve to review both local sources while the daemon is off. Package sources may use plain init, while a Ro Sync projection may use the named marker; the file you do not keep will be archived.`
        : `Use Compare & resolve to review both local sources and script classes while the daemon is off. Ro Sync will archive every marker you do not keep.`,
      path,
      files,
      diagnostic,
    };
  }

  if (
    /PROJECTION_RECOVERY_REQUIRED|pending (?:offline )?projection recovery|projection recovery receipt/i
      .test(diagnostic)
  ) {
    return {
      code: "projection-recovery-required",
      statusLabel: "Recovery required",
      title: "A prior offline repair needs review",
      summary: "Ro Sync found a non-terminal recovery record and kept the daemon stopped.",
      guidance: "Use Review recovery to inspect the durable receipt and affected files before serving this project.",
      path: projectPath,
      files: [],
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
