// Pure normalization and local diff helpers for the daemon-independent
// projection repair flow. Source previews are deliberately bounded again at
// the renderer boundary even though the CLI already enforces its own caps.

const MAX_RENDERED_CONFLICTS = 128;
const MAX_FILES_PER_CONFLICT = 32;
const MAX_PREVIEW_CHARS = 64 * 1024;
const MAX_DIFF_LINES = 240;
const MAX_DIFF_CELLS = 80_000;

function text(value, max = 4096) {
  return String(value ?? "").slice(0, max);
}

function finiteNonNegative(value) {
  const number = Number(value);
  return Number.isFinite(number) && number >= 0 ? number : 0;
}

function exactCount(value) {
  return Number.isSafeInteger(value) && value >= 0 ? value : null;
}

function inferClassName(name) {
  if (/\.(?:server)\.(?:lua|luau)$/i.test(name)) return "Script";
  if (/\.(?:client)\.(?:lua|luau)$/i.test(name)) return "LocalScript";
  return "ModuleScript";
}

function inferStyle(name) {
  if (/^init\.[^.]+$/i.test(name)) return "plain";
  if (/^init \(.+\)/i.test(name)) return "named";
  return "leaf";
}

function normalizeFile(file) {
  const name = text(file?.name, 512);
  const previewValue = typeof file?.preview === "string"
    ? file.preview
    : "";
  const preview = previewValue.slice(0, MAX_PREVIEW_CHARS);
  return {
    name,
    path: text(file?.path, 8192),
    style: text(file?.style || inferStyle(name), 32),
    className: text(file?.className || inferClassName(name), 64),
    size: finiteNonNegative(file?.size),
    sha256: text(file?.sha256, 128),
    preview,
    previewTruncated: !!file?.previewTruncated || preview.length < previewValue.length,
    utf8: file?.utf8 !== false,
  };
}

function normalizeResolution(resolution) {
  if (!resolution || typeof resolution !== "object") return null;
  const recoveryActions = (Array.isArray(resolution.recoveryActions)
    ? resolution.recoveryActions
    : [])
    .filter((action) => action === "resume" || action === "quarantine")
    .slice(0, 2);
  return {
    id: text(resolution.id, 256),
    kind: text(resolution.kind, 64),
    keptFile: text(resolution.keptFile, 512),
    backupPaths: (Array.isArray(resolution.backupPaths)
      ? resolution.backupPaths
      : []).slice(0, MAX_FILES_PER_CONFLICT).map((path) => text(path, 8192)),
    sourcePath: text(resolution.sourcePath, 8192),
    canonicalPath: text(resolution.canonicalPath, 8192),
    receiptPath: text(resolution.receiptPath, 8192),
    receiptAvailable: resolution.receiptAvailable === true,
    recoveryRequired: resolution.recoveryRequired === true,
    recoveryError: text(resolution.recoveryError, 8192),
    recoveryActions: [...new Set(recoveryActions)],
  };
}

export function normalizeProjectionReport(report) {
  const isObject = !!report && typeof report === "object" && !Array.isArray(report);
  const typedFailure = isObject && report.ok === false;
  if (
    !isObject
    || (!typedFailure && (report.ok !== true || !Array.isArray(report.conflicts)))
  ) {
    return {
      ok: false,
      code: "MALFORMED_PROJECTION_REPORT",
      error: "Ro Sync returned an incomplete projection report.",
      project: "",
      conflicts: [],
      remaining: 0,
      totalConflicts: 0,
      countsKnown: false,
      truncated: false,
      resolution: null,
    };
  }
  const rawConflicts = Array.isArray(report.conflicts) ? report.conflicts : [];
  const conflicts = rawConflicts.slice(0, MAX_RENDERED_CONFLICTS).map((conflict) => {
    const files = (Array.isArray(conflict?.files) ? conflict.files : [])
      .slice(0, MAX_FILES_PER_CONFLICT)
      .map(normalizeFile);
    const hashes = new Set(files.map((file) => file.sha256).filter(Boolean));
    const kind = text(conflict?.kind, 64);
    return {
      id: text(conflict?.id, 256),
      kind,
      directory: text(conflict?.directory, 8192),
      sourcePath: text(conflict?.sourcePath, 8192),
      canonicalPath: text(conflict?.canonicalPath, 8192),
      files,
      filesTruncated: (Array.isArray(conflict?.files) ? conflict.files.length : 0) > files.length,
      identical: conflict?.identical === true
        || (files.length > 1 && hashes.size === 1),
    };
  });
  const declaredRemaining = exactCount(report.remaining);
  const declaredTotal = exactCount(report.totalConflicts);
  const reportTruncated = report.truncated === true;
  const resolution = normalizeResolution(report.resolution);
  const normalized = {
    ok: report.ok === true,
    code: text(report.code, 128),
    error: text(report.error, 8192),
    project: text(report?.project, 8192),
    conflicts,
    remaining: declaredRemaining ?? 0,
    totalConflicts: declaredTotal ?? 0,
    countsKnown: report.countsKnown === true,
    truncated: reportTruncated || rawConflicts.length > conflicts.length,
    resolution,
  };
  if (typedFailure) return normalized;

  const completeShape = declaredRemaining !== null
    && declaredTotal !== null
    && normalized.countsKnown
    && typeof report.truncated === "boolean"
    && declaredRemaining === declaredTotal
    && conflicts.length <= declaredTotal
    && (declaredTotal === 0 || conflicts.length > 0)
    && (
      normalized.truncated
        ? conflicts.length < declaredTotal
        : conflicts.length === declaredTotal
    );
  if (!completeShape) {
    return {
      ...normalized,
      ok: false,
      code: "MALFORMED_PROJECTION_REPORT",
      error: "Ro Sync returned contradictory projection completeness metadata.",
    };
  }
  return normalized;
}

export function markerStyleLabel(style) {
  if (style === "plain") return "Package / Rojo marker";
  if (style === "named") return "Ro Sync named marker";
  return "Literal script file";
}

export function formatFileBytes(value) {
  const bytes = finiteNonNegative(value);
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(bytes < 10 * 1024 ? 1 : 0)} KB`;
  return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
}

export function shortHash(value) {
  const hash = text(value, 128);
  return hash ? hash.slice(0, 10) : "unavailable";
}

function positionalDiff(left, right) {
  const rows = [];
  const count = Math.max(left.length, right.length);
  for (let index = 0; index < count; index += 1) {
    const leftText = left[index];
    const rightText = right[index];
    const same = leftText !== undefined && leftText === rightText;
    rows.push({
      left: leftText === undefined
        ? null
        : { number: index + 1, text: leftText, kind: same ? "same" : "remove" },
      right: rightText === undefined
        ? null
        : { number: index + 1, text: rightText, kind: same ? "same" : "add" },
    });
  }
  return rows;
}

// A bounded line LCS keeps the comparison genuinely offline without importing
// a CDN diff package. Large/very-liney previews fall back to a positional
// comparison so malformed source cannot force quadratic renderer work.
export function buildProjectionLineDiff(leftText, rightText) {
  const rawLeft = String(leftText ?? "").split(/\r?\n/);
  const rawRight = String(rightText ?? "").split(/\r?\n/);
  const truncated = rawLeft.length > MAX_DIFF_LINES || rawRight.length > MAX_DIFF_LINES;
  const left = rawLeft.slice(0, MAX_DIFF_LINES);
  const right = rawRight.slice(0, MAX_DIFF_LINES);
  if (left.length * right.length > MAX_DIFF_CELLS) {
    return { rows: positionalDiff(left, right), truncated, approximate: true };
  }

  const width = right.length + 1;
  const table = new Uint16Array((left.length + 1) * width);
  for (let leftIndex = left.length - 1; leftIndex >= 0; leftIndex -= 1) {
    for (let rightIndex = right.length - 1; rightIndex >= 0; rightIndex -= 1) {
      const offset = leftIndex * width + rightIndex;
      table[offset] = left[leftIndex] === right[rightIndex]
        ? table[(leftIndex + 1) * width + rightIndex + 1] + 1
        : Math.max(
            table[(leftIndex + 1) * width + rightIndex],
            table[leftIndex * width + rightIndex + 1],
          );
    }
  }

  const rows = [];
  let leftIndex = 0;
  let rightIndex = 0;
  while (leftIndex < left.length || rightIndex < right.length) {
    if (
      leftIndex < left.length
      && rightIndex < right.length
      && left[leftIndex] === right[rightIndex]
    ) {
      rows.push({
        left: { number: leftIndex + 1, text: left[leftIndex], kind: "same" },
        right: { number: rightIndex + 1, text: right[rightIndex], kind: "same" },
      });
      leftIndex += 1;
      rightIndex += 1;
      continue;
    }
    const removeScore = leftIndex < left.length
      ? table[(leftIndex + 1) * width + rightIndex]
      : -1;
    const addScore = rightIndex < right.length
      ? table[leftIndex * width + rightIndex + 1]
      : -1;
    if (leftIndex < left.length && removeScore >= addScore) {
      rows.push({
        left: { number: leftIndex + 1, text: left[leftIndex], kind: "remove" },
        right: null,
      });
      leftIndex += 1;
    } else {
      rows.push({
        left: null,
        right: { number: rightIndex + 1, text: right[rightIndex], kind: "add" },
      });
      rightIndex += 1;
    }
  }
  return { rows, truncated, approximate: false };
}
