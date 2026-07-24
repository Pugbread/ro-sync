// Pure initial-divergence helpers shared by the modal and policy tests.

export const INITIAL_LIST_RENDER_LIMIT = 300;
export const INITIAL_DETAIL_PAGE_LIMIT = 512;
export const INITIAL_SELECTION_CHUNK_IDS = 2048;
export const INITIAL_DISPLAY_PATH_MAX = 32_768;

export function createChoiceReplayEpochGuard() {
  let globalEpoch = 0;
  const projectEpochs = new Map();
  const epochFor = (projectId) => projectEpochs.get(projectId) || 0;

  return {
    begin(projectId, expectedChoiceId = null) {
      return Object.freeze({
        projectId,
        globalEpoch,
        projectEpoch: epochFor(projectId),
        expectedChoiceId: typeof expectedChoiceId === "string" && expectedChoiceId
          ? expectedChoiceId
          : null,
      });
    },
    invalidate(projectId) {
      projectEpochs.set(projectId, epochFor(projectId) + 1);
    },
    invalidateAll() {
      globalEpoch += 1;
      projectEpochs.clear();
    },
    accepts(token, choiceId) {
      return !!token
        && token.globalEpoch === globalEpoch
        && token.projectEpoch === epochFor(token.projectId)
        && (!token.expectedChoiceId || token.expectedChoiceId === choiceId);
    },
  };
}

export function boundedRenderItems(items, limit = INITIAL_LIST_RENDER_LIMIT) {
  if (!Array.isArray(items) || items.length === 0) {
    return { items: [], hidden: 0 };
  }
  const boundedLimit = Number.isFinite(limit) ? Math.max(0, Math.floor(limit)) : 0;
  const visible = items.slice(0, boundedLimit);
  return {
    items: visible,
    hidden: Math.max(0, items.length - visible.length),
  };
}

// Keep the total number of live file rows bounded across both transfer panes.
// A single busy pane may use the whole budget; when both have content they
// share it without ever exceeding INITIAL_LIST_RENDER_LIMIT.
export function boundedRenderPair(
  available,
  staged,
  limit = INITIAL_LIST_RENDER_LIMIT,
) {
  const left = Array.isArray(available) ? available : [];
  const right = Array.isArray(staged) ? staged : [];
  const boundedLimit = Number.isFinite(limit) ? Math.max(0, Math.floor(limit)) : 0;
  let leftLimit = Math.min(left.length, Math.ceil(boundedLimit / 2));
  let rightLimit = Math.min(right.length, boundedLimit - leftLimit);
  let remaining = boundedLimit - leftLimit - rightLimit;

  if (remaining > 0) {
    const extraLeft = Math.min(remaining, left.length - leftLimit);
    leftLimit += extraLeft;
    remaining -= extraLeft;
  }
  if (remaining > 0) {
    rightLimit += Math.min(remaining, right.length - rightLimit);
  }

  return {
    available: boundedRenderItems(left, leftLimit),
    staged: boundedRenderItems(right, rightLimit),
  };
}

export function choiceSummaryCounts(data) {
  const summary = data?.comparison?.summary || data?.summary || {};
  const create = summaryCount(summary, "create", "newFiles", "new");
  const overwrite = summaryCount(summary, "overwrite", "changedFiles", "changed");
  const remove = summaryCount(summary, "remove", "removedFiles", "removed");
  const summed = create + overwrite + remove;
  const declared = nonNegativeInteger(data?.detailCount);
  return {
    create,
    overwrite,
    remove,
    total: declared ?? summed,
  };
}

export function createChoiceDetailAccumulator(choiceId, totalCount) {
  const id = cleanChoiceId(choiceId);
  const total = nonNegativeInteger(totalCount);
  if (!id) throw new Error("missing choiceId");
  if (total === null) throw new Error("invalid detail count");
  return {
    choiceId: id,
    totalCount: total,
    receivedCount: 0,
    paths: new Set(),
    cursor: null,
    complete: total === 0,
  };
}

// Accept exactly one sequential page. IDs are dense indexes in the daemon's
// immutable, path-sorted detail vector. Validating the full page before
// mutation prevents a malformed response from leaving a half-updated UI.
export function acceptChoiceDetailPage(accumulator, page) {
  if (!accumulator || typeof accumulator !== "object") {
    throw new Error("missing detail accumulator");
  }
  if (!page || typeof page !== "object" || page.ok !== true) {
    throw new Error(cleanText(page?.error) || "detail page rejected");
  }
  if (cleanChoiceId(page.choiceId) !== accumulator.choiceId) {
    throw new Error("detail page belongs to another choice");
  }
  const totalCount = nonNegativeInteger(page.totalCount);
  if (totalCount === null || totalCount !== accumulator.totalCount) {
    throw new Error("detail count changed during paging");
  }
  if (!Array.isArray(page.items)) throw new Error("detail page has no items");

  const normalized = [];
  const pagePaths = new Set();
  for (let index = 0; index < page.items.length; index += 1) {
    const item = normalizeChoiceItem(page.items[index]);
    const expectedId = accumulator.receivedCount + index;
    if (item.id !== expectedId) throw new Error("detail IDs are not sequential");
    if (accumulator.paths.has(item.path) || pagePaths.has(item.path)) {
      throw new Error("detail page repeats a path");
    }
    pagePaths.add(item.path);
    normalized.push(item);
  }

  const complete = page.complete === true;
  const nextLength = accumulator.receivedCount + normalized.length;
  const nextCursor = page.nextCursor == null ? null : cleanCursor(page.nextCursor);
  if (nextLength > totalCount) throw new Error("detail page exceeds declared count");
  if (complete && (nextLength !== totalCount || nextCursor !== null)) {
    throw new Error("complete detail page is inconsistent");
  }
  if (!complete && (nextLength >= totalCount || !nextCursor || normalized.length === 0)) {
    throw new Error("incomplete detail page cannot advance");
  }

  accumulator.receivedCount = nextLength;
  for (const path of pagePaths) accumulator.paths.add(path);
  accumulator.cursor = nextCursor;
  accumulator.complete = complete;
  return normalized;
}

export function choiceDetailsPath(
  choiceId,
  cursor = null,
  limit = INITIAL_DETAIL_PAGE_LIMIT,
) {
  const id = cleanChoiceId(choiceId);
  if (!id) throw new Error("missing choiceId");
  const pageLimit = Math.min(
    1024,
    Math.max(1, Number.isFinite(limit) ? Math.floor(limit) : INITIAL_DETAIL_PAGE_LIMIT),
  );
  const query = new URLSearchParams({
    choiceId: id,
    limit: String(pageLimit),
  });
  if (cursor != null && String(cursor) !== "") query.set("cursor", cleanCursor(cursor));
  return `/initial-choice/details?${query.toString()}`;
}

export function selectedTransferIds(items, selected) {
  const allowed = new Set(
    (Array.isArray(items) ? items : [])
      .map((item) => nonNegativeInteger(item?.id))
      .filter((id) => id !== null),
  );
  return [...(selected || [])]
    .map((id) => nonNegativeInteger(id))
    .filter((id) => id !== null && allowed.has(id))
    .sort((left, right) => left - right);
}

export function selectionIdChunks(ids, chunkSize = INITIAL_SELECTION_CHUNK_IDS) {
  const size = Math.min(
    INITIAL_SELECTION_CHUNK_IDS,
    Math.max(1, Number.isFinite(chunkSize) ? Math.floor(chunkSize) : INITIAL_SELECTION_CHUNK_IDS),
  );
  const unique = [...new Set(
    (Array.isArray(ids) ? ids : [])
      .map((id) => nonNegativeInteger(id))
      .filter((id) => id !== null),
  )].sort((left, right) => left - right);
  const chunks = [];
  for (let index = 0; index < unique.length; index += size) {
    chunks.push(unique.slice(index, index + size));
  }
  return chunks;
}

export function divergenceItems(comparison) {
  if (!comparison || typeof comparison !== "object") return [];
  const items = [];
  append(items, comparison.newFiles, "create");
  append(items, comparison.changedFiles, "overwrite");
  append(items, comparison.removedFiles, "remove");
  const unique = new Map();
  for (const item of items) {
    if (!unique.has(item.path)) unique.set(item.path, item);
  }
  return [...unique.values()].sort((left, right) =>
    left.path.localeCompare(right.path, undefined, { numeric: true })
  );
}

// "ReplicatedStorage/Shared/CarPhysics" → parent "ReplicatedStorage/Shared",
// name "CarPhysics". The name is what users scan for, so rows render it
// prominently and ellipsize the parent instead.
export function splitDisplayPath(path) {
  const value = String(path || "");
  const cut = value.lastIndexOf("/");
  if (cut <= 0) return { parent: "", name: value };
  return { parent: value.slice(0, cut), name: value.slice(cut + 1) };
}

export function itemClassLabel(item) {
  return (
    item?.localClass
    || item?.class
    || item?.studioClass
    || (item?.kind === "folder" ? "Folder" : "Synced item")
  );
}

// The action a staged item performs in Studio. "Differs" items are two-sided
// — the compare cannot know whether Studio or disk made the edit — so the
// verb must say that staging makes the DISK copy win.
export function transferVerb(item) {
  if (item?.action === "create") return "Create in Studio";
  if (item?.action === "remove") return "Remove from Studio";
  return "Replace Studio version with disk";
}

// The state of a divergent path, without implying which side edited it.
export function itemStateLabel(item) {
  if (item?.action === "create") return "Only on disk";
  if (item?.action === "remove") return "Missing on disk";
  return "Differs from Studio";
}

export function transferMeta(item) {
  const cls = item?.localClass || item?.class || item?.studioClass || "Synced item";
  if (item?.kind === "folder") return `${cls} tree`;
  if (item?.action === "remove") return `${cls} · absent on disk`;
  if (item?.classChanged) return `${item.studioClass || "Studio type"} → ${item.localClass || cls}`;
  if (item?.sourceChanged) return `${cls} · source differs`;
  return cls;
}

function append(target, source, action) {
  if (!Array.isArray(source)) return;
  for (const raw of source) {
    if (!raw || typeof raw !== "object") continue;
    const path = cleanPath(raw.path);
    if (!path) continue;
    target.push({
      action,
      path,
      kind: cleanText(raw.kind),
      class: cleanText(raw.class),
      localClass: cleanText(raw.localClass),
      studioClass: cleanText(raw.studioClass),
      classChanged: raw.classChanged === true,
      sourceChanged: raw.sourceChanged === true,
    });
  }
}

function cleanPath(value) {
  const path = String(value || "").replace(/[\u0000-\u001f\u007f]/g, " ").trim();
  return path.length <= INITIAL_DISPLAY_PATH_MAX ? path : "";
}

function cleanText(value) {
  return String(value || "").replace(/[\u0000-\u001f\u007f]/g, " ").trim().slice(0, 128);
}

function cleanChoiceId(value) {
  if (
    typeof value !== "string"
    || value.length === 0
    || value.length > 256
    || /[\u0000-\u001f\u007f]/.test(value)
  ) return "";
  return value;
}

function cleanCursor(value) {
  if (
    typeof value !== "string"
    || value.length === 0
    || value.length > 4096
    || /[\u0000-\u001f\u007f]/.test(value)
  ) {
    throw new Error("invalid detail cursor");
  }
  return value;
}

function nonNegativeInteger(value) {
  const number = Number(value);
  return Number.isSafeInteger(number) && number >= 0 ? number : null;
}

function summaryCount(summary, ...keys) {
  for (const key of keys) {
    const value = nonNegativeInteger(summary?.[key]);
    if (value !== null) return value;
  }
  return 0;
}

function normalizeChoiceItem(raw) {
  if (!raw || typeof raw !== "object") throw new Error("invalid detail item");
  const id = typeof raw.id === "number" ? nonNegativeInteger(raw.id) : null;
  const action = cleanText(raw.action);
  const path = cleanPath(raw.path);
  if (id === null || !["create", "overwrite", "remove"].includes(action) || !path) {
    throw new Error("invalid detail item");
  }
  return {
    id,
    action,
    path,
    kind: cleanText(raw.kind),
    class: cleanText(raw.class),
    localClass: cleanText(raw.localClass),
    studioClass: cleanText(raw.studioClass),
    classChanged: raw.classChanged === true,
    sourceChanged: raw.sourceChanged === true,
  };
}
