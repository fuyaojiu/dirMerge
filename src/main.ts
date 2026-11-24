import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { open } from "@tauri-apps/plugin-dialog";

type MergeReport = {
  copiedFiles: number;
  movedFiles: number;
  createdDirs: number;
  mergedDirs: number;
  duplicateFiles: number;
  duplicateSizeBytes: number;
  duplicateEntries: DuplicateEntry[];
  pathConflicts: number;
  skippedCount: number;
  skippedEntries: string[];
};

type DuplicateEntry = {
  pathSegments: string[];
  sizeBytes: number;
};

type DuplicateNode = {
  name: string;
  isDir: boolean;
  sizeBytes: number;
  fileCount: number;
  children: Map<string, DuplicateNode>;
};

type OperationMode = "copy" | "move";

type ProgressStage = "scanning" | "merging" | "finished";
type SearchProgressStage = "counting" | "searching" | "cancelled" | "finished";

type MergeProgress = {
  total: number;
  completed: number;
  currentPath: string;
  stage: ProgressStage;
};

type SearchProgress = {
  total: number;
  scannedEntries: number;
  matchedEntries: number;
  skippedCount: number;
  currentPath: string;
  stage: SearchProgressStage;
};

type MergeErrorPayload = {
  message: string;
  details: string;
  backtrace: string;
};

type AppStatus = "empty" | "ready" | "running" | "success" | "error";
type ActiveTab = "merge" | "search";
type SearchSortField = "kind" | "size" | "name";
type SortDirection = "asc" | "desc";
type SearchSortRule = {
  field: SearchSortField;
  direction: SortDirection;
};

type SearchResult = {
  name: string;
  path: string;
  parentPath: string;
  isDir: boolean;
  sizeBytes: number;
};

type SearchReport = {
  results: SearchResult[];
  scannedEntries: number;
  skippedCount: number;
  truncated: boolean;
  cancelled: boolean;
};

const state = {
  activeTab: "merge" as ActiveTab,
  originalPath: "",
  mergePath: "",
  searchRootPath: "",
  isSearching: false,
  isCancellingSearch: false,
  isMerging: false,
  operationMode: "move" as OperationMode,
  archiveDuplicates: false,
  lastErrorDetails: "",
  pendingProgress: null as MergeProgress | null,
  pendingSearchProgress: null as SearchProgress | null,
  progressFrame: 0,
  searchProgressFrame: 0,
  skippedEntries: [] as string[],
  skippedScrollHandler: null as (() => void) | null,
  rawSearchResults: [] as SearchResult[],
  searchResults: [] as SearchResult[],
  searchScrollHandler: null as (() => void) | null,
  searchSortRules: [{ field: "kind", direction: "asc" }] as SearchSortRule[],
};

const SAME_DIRECTORY_MESSAGE = "原始文件夹和要合并的文件夹不能相同。";

function requireElement<T extends HTMLElement>(selector: string): T {
  const element = document.querySelector<T>(selector);

  if (!element) {
    throw new Error(`Missing element: ${selector}`);
  }

  return element;
}

function setPathOutput(selector: string, path: string) {
  const output = requireElement<HTMLOutputElement>(selector);

  output.value = path;
  output.textContent = path || "未选择";
  output.classList.toggle("is-empty", !path);
}

function setActiveTab(tab: ActiveTab) {
  state.activeTab = tab;
  const isMergeActive = tab === "merge";

  requireElement<HTMLElement>("#merge-view").hidden = !isMergeActive;
  requireElement<HTMLElement>("#search-view").hidden = isMergeActive;
  requireElement<HTMLButtonElement>("#merge-tab").ariaSelected =
    String(isMergeActive);
  requireElement<HTMLButtonElement>("#search-tab").ariaSelected =
    String(!isMergeActive);
}

function setPath(kind: "original" | "merge", path: string) {
  const key = kind === "original" ? "originalPath" : "mergePath";

  state[key] = path;
  setPathOutput(kind === "original" ? "#original-path" : "#merge-path", path);
  validateSelection();
}

function setSearchRootPath(path: string) {
  state.searchRootPath = path;
  setPathOutput("#search-root-path", path);
  updateSearchButton();
}

function updateMergeButton() {
  const mergeButton = requireElement<HTMLButtonElement>("#merge-button");
  mergeButton.disabled =
    state.isMerging ||
    !state.originalPath ||
    !state.mergePath ||
    isSameDirectorySelected();
  mergeButton.textContent = state.isMerging ? "合并中..." : "开始合并";
}

function updateSearchButton() {
  const searchButton = requireElement<HTMLButtonElement>("#search-button");
  const cancelSearchButton =
    requireElement<HTMLButtonElement>("#cancel-search-button");
  const includeFiles = requireElement<HTMLInputElement>("#include-files").checked;
  const includeDirectories =
    requireElement<HTMLInputElement>("#include-directories").checked;

  searchButton.disabled =
    state.isSearching || !state.searchRootPath || (!includeFiles && !includeDirectories);
  searchButton.textContent = state.isSearching ? "搜索中..." : "开始搜索";
  cancelSearchButton.hidden = !state.isSearching;
  cancelSearchButton.disabled = state.isCancellingSearch;
  cancelSearchButton.textContent = state.isCancellingSearch ? "停止中..." : "停止";
}

function isSameDirectorySelected() {
  return (
    Boolean(state.originalPath) &&
    Boolean(state.mergePath) &&
    state.originalPath === state.mergePath
  );
}

function validateSelection() {
  updateMergeButton();

  if (isSameDirectorySelected()) {
    clearErrorDetails();
    setStatus(SAME_DIRECTORY_MESSAGE, "error");
    return false;
  }

  return true;
}

function getStatusDetail(status: AppStatus, message: string) {
  if (status === "empty") {
    return "请选择原始文件夹和要合并的文件夹";
  }

  if (status === "ready") {
    return "配置已就绪，可以开始合并";
  }

  if (status === "running") {
    return "正在处理文件，请保持应用打开";
  }

  if (status === "success") {
    return "任务已完成，结果已更新";
  }

  return message === SAME_DIRECTORY_MESSAGE
    ? "请选择两个不同的文件夹"
    : "点击详情查看完整错误信息";
}

function setStatus(message: string, status: AppStatus = "empty") {
  const statusPanel = requireElement<HTMLElement>("#status-message");
  const detailButton = requireElement<HTMLButtonElement>("#status-detail-button");

  statusPanel.dataset.state = status;
  requireElement<HTMLElement>("#status-title").textContent = message;
  requireElement<HTMLElement>("#status-detail").textContent = getStatusDetail(
    status,
    message,
  );
  detailButton.hidden = status !== "error" || !state.lastErrorDetails;
}

function setErrorStatus(error: unknown) {
  const parsedError = parseMergeError(error);
  state.lastErrorDetails = formatErrorDetails(parsedError);
  setStatus(parsedError.message, "error");
}

function parseMergeError(error: unknown): MergeErrorPayload {
  const rawMessage = error instanceof Error ? error.message : String(error);

  try {
    const parsed = JSON.parse(rawMessage) as Partial<MergeErrorPayload>;

    if (typeof parsed.message === "string") {
      return {
        message: parsed.message,
        details: typeof parsed.details === "string" ? parsed.details : rawMessage,
        backtrace: typeof parsed.backtrace === "string" ? parsed.backtrace : "",
      };
    }
  } catch {
    // Keep raw string fallback below.
  }

  return {
    message: rawMessage,
    details: rawMessage,
    backtrace: error instanceof Error && error.stack ? error.stack : "",
  };
}

function formatErrorDetails(error: MergeErrorPayload) {
  return [
    `Message:\n${error.message}`,
    `Details:\n${error.details || "无"}`,
    `Backtrace:\n${error.backtrace || "无"}`,
  ].join("\n\n");
}

function clearErrorDetails() {
  state.lastErrorDetails = "";
}

function openErrorDialog() {
  if (!state.lastErrorDetails) {
    return;
  }

  requireElement<HTMLElement>("#error-details").textContent =
    state.lastErrorDetails;
  requireElement<HTMLDialogElement>("#error-dialog").showModal();
}

function clearReport() {
  requireElement<HTMLElement>("#report").hidden = true;
  requireElement<HTMLElement>("#duplicate-panel").hidden = true;
  requireElement<HTMLElement>("#duplicate-tree").replaceChildren();
  requireElement<HTMLElement>("#skipped-list").replaceChildren();
  resetSkippedVirtualList();
}

function resetProgress() {
  renderProgress({
    total: 0,
    completed: 0,
    currentPath: "",
    stage: "scanning",
  });
  requireElement<HTMLElement>("#progress-panel").hidden = true;
}

function resetSearchProgress() {
  renderSearchProgress({
    total: 0,
    scannedEntries: 0,
    matchedEntries: 0,
    skippedCount: 0,
    currentPath: "",
    stage: "counting",
  });
  requireElement<HTMLElement>("#search-progress-panel").hidden = true;
}

function renderProgress(progress: MergeProgress) {
  const progressPanel = requireElement<HTMLElement>("#progress-panel");
  const progressBar = requireElement<HTMLProgressElement>("#merge-progress");
  const total = Math.max(progress.total, 0);
  const completed = Math.min(Math.max(progress.completed, 0), total);
  const percent = total > 0 ? Math.round((completed / total) * 100) : 0;

  progressPanel.hidden = false;
  progressBar.max = total > 0 ? total : 100;
  progressBar.value = total > 0 ? completed : 0;
  requireElement<HTMLElement>("#progress-percent").textContent = `${percent}%`;
  requireElement<HTMLElement>("#progress-completed").textContent = String(completed);
  requireElement<HTMLElement>("#progress-total").textContent = String(total);
  requireElement<HTMLElement>("#progress-current").textContent =
    progress.currentPath || "";
  requireElement<HTMLElement>("#progress-stage").textContent =
    progress.stage === "scanning"
      ? "扫描中"
      : progress.stage === "finished"
        ? "已完成"
        : "合并中";
}

function scheduleProgressRender(progress: MergeProgress) {
  state.pendingProgress = progress;

  if (state.progressFrame) {
    return;
  }

  state.progressFrame = requestAnimationFrame(() => {
    state.progressFrame = 0;

    if (state.pendingProgress) {
      renderProgress(state.pendingProgress);
      state.pendingProgress = null;
    }
  });
}

function renderSearchProgress(progress: SearchProgress) {
  const progressPanel = requireElement<HTMLElement>("#search-progress-panel");
  const progressBar = requireElement<HTMLProgressElement>("#search-progress");
  const total = Math.max(progress.total, 0);
  const scanned = Math.min(Math.max(progress.scannedEntries, 0), total);
  const percent = total > 0 ? Math.round((scanned / total) * 100) : 0;

  progressPanel.hidden = false;
  progressBar.max = total > 0 ? total : 100;
  progressBar.value = total > 0 ? scanned : 0;
  requireElement<HTMLElement>("#search-progress-percent").textContent = `${percent}%`;
  requireElement<HTMLElement>("#search-progress-scanned").textContent = String(scanned);
  requireElement<HTMLElement>("#search-progress-total").textContent = String(total);
  requireElement<HTMLElement>("#search-progress-matched").textContent = String(
    progress.matchedEntries,
  );
  requireElement<HTMLElement>("#search-progress-skipped").textContent = String(
    progress.skippedCount,
  );
  requireElement<HTMLElement>("#search-progress-current").textContent =
    progress.currentPath || "";
  requireElement<HTMLElement>("#search-progress-stage").textContent =
    progress.stage === "counting"
      ? "统计中"
      : progress.stage === "cancelled"
        ? "已停止"
      : progress.stage === "finished"
        ? "已完成"
        : "搜索中";
}

function scheduleSearchProgressRender(progress: SearchProgress) {
  state.pendingSearchProgress = progress;

  if (state.searchProgressFrame) {
    return;
  }

  state.searchProgressFrame = requestAnimationFrame(() => {
    state.searchProgressFrame = 0;

    if (state.pendingSearchProgress) {
      renderSearchProgress(state.pendingSearchProgress);
      state.pendingSearchProgress = null;
    }
  });
}

function renderReport(report: MergeReport) {
  requireElement<HTMLElement>("#report").hidden = false;
  requireElement<HTMLElement>("#copied-files").textContent = String(
    state.operationMode === "copy" ? report.copiedFiles : report.movedFiles,
  );
  requireElement<HTMLElement>("#created-dirs").textContent = String(
    report.createdDirs,
  );
  requireElement<HTMLElement>("#merged-dirs").textContent = String(
    report.mergedDirs,
  );
  requireElement<HTMLElement>("#duplicate-files").textContent = String(
    report.duplicateFiles,
  );
  requireElement<HTMLElement>("#duplicate-size").textContent = formatBytes(
    report.duplicateSizeBytes,
  );
  requireElement<HTMLElement>("#path-conflicts").textContent = String(
    report.pathConflicts,
  );
  renderDuplicateTree(report);

  const skippedPanel = requireElement<HTMLElement>("#skipped-panel");
  const skippedTitle = requireElement<HTMLElement>("#skipped-title");
  skippedTitle.textContent = `已跳过 ${report.skippedCount} 项`;

  skippedPanel.hidden = report.skippedCount === 0;

  if (report.skippedCount > 0) {
    setupSkippedVirtualList(report.skippedEntries);
  }
}

const duplicateAutoExpandLimit = 200;
const duplicateChildrenPageSize = 500;

function buildDuplicateTree(entries: DuplicateEntry[]) {
  const root: DuplicateNode = {
    name: "",
    isDir: true,
    sizeBytes: 0,
    fileCount: 0,
    children: new Map(),
  };

  for (const entry of entries) {
    if (entry.pathSegments.length === 0) {
      continue;
    }

    let node = root;
    node.sizeBytes += entry.sizeBytes;
    node.fileCount += 1;

    entry.pathSegments.forEach((segment, index) => {
      const isDir = index < entry.pathSegments.length - 1;
      // 目录和文件分开存放，避免同名的目录与文件互相覆盖。
      const key = `${isDir ? "d" : "f"}:${segment}`;
      let child = node.children.get(key);

      if (!child) {
        child = {
          name: segment,
          isDir,
          sizeBytes: 0,
          fileCount: 0,
          children: new Map(),
        };
        node.children.set(key, child);
      }

      child.sizeBytes += entry.sizeBytes;
      child.fileCount += 1;
      node = child;
    });
  }

  return root;
}

function sortedDuplicateChildren(node: DuplicateNode) {
  return [...node.children.values()].sort(
    (first, second) =>
      Number(second.isDir) - Number(first.isDir) ||
      first.name.localeCompare(second.name, "zh-Hans", {
        numeric: true,
        sensitivity: "base",
      }),
  );
}

function createDuplicateLabel(name: string, meta: string) {
  const nameElement = document.createElement("span");
  nameElement.className = "duplicate-name";
  nameElement.textContent = name;
  nameElement.title = name;

  const metaElement = document.createElement("span");
  metaElement.className = "duplicate-meta";
  metaElement.textContent = meta;

  return [nameElement, metaElement];
}

function appendDuplicateChildren(
  container: HTMLElement,
  children: DuplicateNode[],
  startIndex: number,
  autoExpand: boolean,
) {
  const endIndex = Math.min(children.length, startIndex + duplicateChildrenPageSize);
  const fragment = document.createDocumentFragment();

  for (let index = startIndex; index < endIndex; index += 1) {
    fragment.append(createDuplicateNodeElement(children[index], autoExpand));
  }

  if (endIndex < children.length) {
    const moreButton = document.createElement("button");
    moreButton.className = "duplicate-more";
    moreButton.type = "button";
    moreButton.textContent = `显示更多（还有 ${children.length - endIndex} 项）`;
    moreButton.addEventListener("click", () => {
      moreButton.remove();
      appendDuplicateChildren(container, children, endIndex, autoExpand);
    });
    fragment.append(moreButton);
  }

  container.append(fragment);
}

function createDuplicateNodeElement(node: DuplicateNode, autoExpand: boolean) {
  if (!node.isDir) {
    const file = document.createElement("div");
    file.className = "duplicate-file";
    file.setAttribute("role", "treeitem");
    file.append(...createDuplicateLabel(node.name, formatBytes(node.sizeBytes)));
    return file;
  }

  const details = document.createElement("details");
  details.setAttribute("role", "treeitem");
  const summary = document.createElement("summary");
  summary.append(
    ...createDuplicateLabel(
      node.name,
      `${node.fileCount} 个文件 · ${formatBytes(node.sizeBytes)}`,
    ),
  );
  const childrenContainer = document.createElement("div");
  childrenContainer.className = "duplicate-children";
  childrenContainer.setAttribute("role", "group");
  details.append(summary, childrenContainer);

  // 子节点在首次展开时才生成，重复文件很多时也不会卡住界面。
  let populated = false;
  const populate = () => {
    if (populated) {
      return;
    }

    populated = true;
    appendDuplicateChildren(
      childrenContainer,
      sortedDuplicateChildren(node),
      0,
      autoExpand,
    );
  };

  details.addEventListener("toggle", () => {
    if (details.open) {
      populate();
    }
  });

  if (autoExpand) {
    populate();
    details.open = true;
  }

  return details;
}

function renderDuplicateTree(report: MergeReport) {
  const panel = requireElement<HTMLElement>("#duplicate-panel");
  const tree = requireElement<HTMLElement>("#duplicate-tree");
  const entries = report.duplicateEntries ?? [];

  tree.replaceChildren();
  panel.hidden = entries.length === 0;

  if (entries.length === 0) {
    return;
  }

  requireElement<HTMLElement>("#duplicate-title").textContent =
    `重复文件 ${report.duplicateFiles} 个，共 ${formatBytes(report.duplicateSizeBytes)}`;
  appendDuplicateChildren(
    tree,
    sortedDuplicateChildren(buildDuplicateTree(entries)),
    0,
    entries.length <= duplicateAutoExpandLimit,
  );
}

const skippedRowHeight = 26;
const skippedOverscan = 8;
const searchOverscan = 8;

function resetSkippedVirtualList() {
  const viewport = requireElement<HTMLElement>("#skipped-viewport");
  const spacer = requireElement<HTMLElement>("#skipped-spacer");

  if (state.skippedScrollHandler) {
    viewport.removeEventListener("scroll", state.skippedScrollHandler);
    state.skippedScrollHandler = null;
  }

  state.skippedEntries = [];
  viewport.scrollTop = 0;
  spacer.style.height = "0px";
  requireElement<HTMLElement>("#skipped-list").replaceChildren();
}

function setupSkippedVirtualList(entries: string[]) {
  resetSkippedVirtualList();

  state.skippedEntries = entries;
  const viewport = requireElement<HTMLElement>("#skipped-viewport");
  const spacer = requireElement<HTMLElement>("#skipped-spacer");
  spacer.style.height = `${entries.length * skippedRowHeight}px`;

  const renderVisibleRows = () => renderSkippedVisibleRows(viewport);
  state.skippedScrollHandler = renderVisibleRows;
  viewport.addEventListener("scroll", renderVisibleRows);
  renderVisibleRows();
}

function renderSkippedVisibleRows(viewport: HTMLElement) {
  const list = requireElement<HTMLUListElement>("#skipped-list");
  const viewportHeight = viewport.clientHeight;
  const startIndex = Math.max(
    0,
    Math.floor(viewport.scrollTop / skippedRowHeight) - skippedOverscan,
  );
  const visibleCount = Math.ceil(viewportHeight / skippedRowHeight) + skippedOverscan * 2;
  const endIndex = Math.min(state.skippedEntries.length, startIndex + visibleCount);
  const fragment = document.createDocumentFragment();

  for (let index = startIndex; index < endIndex; index += 1) {
    const item = document.createElement("li");
    item.textContent = state.skippedEntries[index];
    fragment.append(item);
  }

  list.style.transform = `translateY(${startIndex * skippedRowHeight}px)`;
  list.replaceChildren(fragment);
}

function clearSearchReport() {
  requireElement<HTMLElement>("#search-report").hidden = true;
  resetSearchResults();
}

function renderSearchReport(report: SearchReport) {
  const title = requireElement<HTMLElement>("#search-result-title");

  requireElement<HTMLElement>("#search-report").hidden = false;
  requireElement<HTMLElement>("#search-scanned").textContent = String(
    report.scannedEntries,
  );
  requireElement<HTMLElement>("#search-skipped").textContent = String(
    report.skippedCount,
  );
  title.textContent = report.truncated
    ? `搜索结果 ${report.results.length}+ 项`
    : report.cancelled
      ? `已停止，保留结果 ${report.results.length} 项`
    : `搜索结果 ${report.results.length} 项`;
  requireElement<HTMLElement>("#search-empty").hidden = report.results.length > 0;
  state.rawSearchResults = report.results;
  applySearchSort();
}

function resetSearchVirtualList() {
  const viewport = requireElement<HTMLElement>("#search-result-viewport");
  const spacer = requireElement<HTMLElement>("#search-result-spacer");

  if (state.searchScrollHandler) {
    viewport.removeEventListener("scroll", state.searchScrollHandler);
    state.searchScrollHandler = null;
  }

  state.searchResults = [];
  viewport.scrollTop = 0;
  spacer.style.height = "0px";
  requireElement<HTMLElement>("#search-empty").hidden = true;
  requireElement<HTMLElement>("#search-result-list").replaceChildren();
}

function resetSearchResults() {
  state.rawSearchResults = [];
  resetSearchVirtualList();
}

function setupSearchVirtualList(results: SearchResult[]) {
  resetSearchVirtualList();

  state.searchResults = results;
  const viewport = requireElement<HTMLElement>("#search-result-viewport");
  const spacer = requireElement<HTMLElement>("#search-result-spacer");
  const searchRowHeight = getSearchRowHeight();
  spacer.style.height = `${results.length * searchRowHeight}px`;

  const renderVisibleRows = () => renderSearchVisibleRows(viewport);
  state.searchScrollHandler = renderVisibleRows;
  viewport.addEventListener("scroll", renderVisibleRows);
  renderVisibleRows();
}

function renderSearchVisibleRows(viewport: HTMLElement) {
  const list = requireElement<HTMLUListElement>("#search-result-list");
  const viewportHeight = viewport.clientHeight;
  const searchRowHeight = getSearchRowHeight();
  const startIndex = Math.max(
    0,
    Math.floor(viewport.scrollTop / searchRowHeight) - searchOverscan,
  );
  const visibleCount =
    Math.ceil(viewportHeight / searchRowHeight) + searchOverscan * 2;
  const endIndex = Math.min(state.searchResults.length, startIndex + visibleCount);
  const fragment = document.createDocumentFragment();

  for (let index = startIndex; index < endIndex; index += 1) {
    fragment.append(createSearchResultItem(state.searchResults[index]));
  }

  list.style.transform = `translateY(${startIndex * searchRowHeight}px)`;
  list.replaceChildren(fragment);
}

function applySearchSort() {
  if (state.rawSearchResults.length === 0) {
    resetSearchVirtualList();
    updateSortHeaders();
    return;
  }

  setupSearchVirtualList(sortSearchResults(state.rawSearchResults));
  updateSortHeaders();
}

function sortSearchResults(results: SearchResult[]) {
  return [...results].sort((first, second) => {
    for (const rule of state.searchSortRules) {
      const compared = compareSearchResults(first, second, rule.field);

      if (compared !== 0) {
        return compared * (rule.direction === "asc" ? 1 : -1);
      }
    }

    return (
      compareSearchResults(first, second, "name") ||
      compareSearchResults(first, second, "kind") ||
      compareSearchResults(first, second, "size")
    );
  });
}

function toggleSearchSort(field: SearchSortField, append: boolean) {
  const existingIndex = state.searchSortRules.findIndex(
    (rule) => rule.field === field,
  );
  const existingRule =
    existingIndex >= 0 ? state.searchSortRules[existingIndex] : null;
  const nextRule: SearchSortRule = {
    field,
    direction: existingRule?.direction === "asc" ? "desc" : "asc",
  };

  if (append) {
    const nextRules = state.searchSortRules.filter((rule) => rule.field !== field);
    nextRules.push(nextRule);
    state.searchSortRules = nextRules;
  } else {
    state.searchSortRules = [nextRule];
  }

  applySearchSort();
}

function updateSortHeaders() {
  for (const heading of document.querySelectorAll<HTMLButtonElement>(
    "[data-sort-field]",
  )) {
    const field = heading.dataset.sortField as SearchSortField | undefined;
    const ruleIndex = state.searchSortRules.findIndex(
      (rule) => rule.field === field,
    );
    const indicator = heading.querySelector<HTMLElement>(".sort-indicator");

    heading.classList.toggle("is-active", ruleIndex >= 0);
    heading.setAttribute(
      "aria-sort",
      ruleIndex >= 0
        ? state.searchSortRules[ruleIndex].direction === "asc"
          ? "ascending"
          : "descending"
        : "none",
    );

    if (indicator) {
      indicator.textContent =
        ruleIndex >= 0
          ? `${state.searchSortRules[ruleIndex].direction === "asc" ? "↑" : "↓"}${ruleIndex + 1}`
          : "";
    }
  }
}

function compareSearchResults(
  first: SearchResult,
  second: SearchResult,
  field: SearchSortField,
) {
  if (field === "kind") {
    return Number(second.isDir) - Number(first.isDir);
  }

  if (field === "size") {
    return first.sizeBytes - second.sizeBytes;
  }

  return first.name.localeCompare(second.name, "zh-Hans", {
    numeric: true,
    sensitivity: "base",
  });
}

function getSearchRowHeight() {
  const rawValue = getComputedStyle(document.documentElement)
    .getPropertyValue("--search-row-height")
    .trim();
  const parsedValue = Number.parseFloat(rawValue);

  return Number.isFinite(parsedValue) && parsedValue > 0 ? parsedValue : 58;
}

function createSearchResultItem(result: SearchResult) {
  const item = document.createElement("li");
  item.className = "search-result-item";
  item.setAttribute("role", "row");

  const type = document.createElement("span");
  type.className = "table-cell result-type-cell";
  type.setAttribute("role", "cell");
  const typeBadge = document.createElement("span");
  typeBadge.className = "result-type";
  typeBadge.textContent = result.isDir ? "文件夹" : "文件";
  type.append(typeBadge);

  const name = document.createElement("strong");
  name.className = "table-cell result-name";
  name.setAttribute("role", "cell");
  name.textContent = result.name;

  const path = document.createElement("span");
  path.className = "table-cell result-path";
  path.setAttribute("role", "cell");
  path.textContent = result.path;

  const size = document.createElement("span");
  size.className = "table-cell result-size";
  size.setAttribute("role", "cell");
  size.textContent = formatBytes(result.sizeBytes);

  const action = document.createElement("span");
  action.className = "table-cell result-action";
  action.setAttribute("role", "cell");
  const button = document.createElement("button");
  button.className = "result-open-button";
  button.type = "button";
  button.textContent = "打开位置";
  button.addEventListener("click", () => revealSearchResult(result.path));
  const deleteButton = document.createElement("button");
  deleteButton.className = "result-delete-button";
  deleteButton.type = "button";
  deleteButton.textContent = "删除";
  deleteButton.addEventListener("click", () => deleteSearchResult(result.path));
  action.append(button, deleteButton);

  item.append(type, name, size, path, action);
  return item;
}

function formatBytes(bytes: number) {
  if (!Number.isFinite(bytes) || bytes <= 0) {
    return "0 B";
  }

  const units = ["B", "KB", "MB", "GB", "TB"];
  let value = bytes;
  let unitIndex = 0;

  while (value >= 1024 && unitIndex < units.length - 1) {
    value /= 1024;
    unitIndex += 1;
  }

  const precision = value >= 100 || unitIndex === 0 ? 0 : value >= 10 ? 1 : 2;
  return `${value.toFixed(precision)} ${units[unitIndex]}`;
}

function getSelectedOperationMode() {
  return requireElement<HTMLInputElement>(
    'input[name="operation-mode"]:checked',
  ).value as OperationMode;
}

function syncOptionsFromControls() {
  state.operationMode = getSelectedOperationMode();
  state.archiveDuplicates =
    requireElement<HTMLInputElement>("#archive-duplicates").checked;
  requireElement<HTMLElement>("#processed-files-label").textContent =
    state.operationMode === "copy" ? "复制文件" : "移动文件";
}

async function chooseDirectory(kind: "original" | "merge") {
  const selected = await open({
    directory: true,
    multiple: false,
    title: kind === "original" ? "选择原始文件夹" : "选择要合并的文件夹",
  });

  if (typeof selected === "string") {
    setPath(kind, selected);
    clearReport();
    resetProgress();

    if (!isSameDirectorySelected()) {
      clearErrorDetails();
      setStatus(
        state.originalPath && state.mergePath ? "等待合并" : "等待选择文件夹",
        state.originalPath && state.mergePath ? "ready" : "empty",
      );
    }
  }
}

async function chooseSearchRootDirectory() {
  const selected = await open({
    directory: true,
    multiple: false,
    title: "选择搜索文件夹",
  });

  if (typeof selected === "string") {
    setSearchRootPath(selected);
    clearSearchReport();
    resetSearchProgress();
    clearErrorDetails();
    setStatus("等待搜索", "ready");
  }
}

async function mergeDirectories() {
  if (!state.originalPath || !state.mergePath) {
    clearErrorDetails();
    setStatus("请先选择两个文件夹。", "error");
    return;
  }

  if (!validateSelection()) {
    return;
  }

  state.isMerging = true;
  syncOptionsFromControls();
  updateMergeButton();
  clearReport();
  resetProgress();
  requireElement<HTMLElement>("#progress-panel").hidden = false;
  setStatus("正在合并", "running");

  try {
    const report = await invoke<MergeReport>("merge_directories", {
      originalPath: state.originalPath,
      mergePath: state.mergePath,
      operationMode: state.operationMode,
      archiveDuplicates: state.archiveDuplicates,
    });

    renderReport(report);
    clearErrorDetails();
    setStatus("合并完成", "success");
  } catch (error) {
    setErrorStatus(error);
  } finally {
    state.isMerging = false;
    updateMergeButton();
  }
}

async function searchFolder() {
  if (!state.searchRootPath) {
    clearErrorDetails();
    setStatus("请先选择搜索文件夹。", "error");
    return;
  }

  const includeFiles = requireElement<HTMLInputElement>("#include-files").checked;
  const includeDirectories =
    requireElement<HTMLInputElement>("#include-directories").checked;

  if (!includeFiles && !includeDirectories) {
    clearErrorDetails();
    setStatus("请至少选择一种搜索类型。", "error");
    return;
  }

  state.isSearching = true;
  state.isCancellingSearch = false;
  updateSearchButton();
  clearSearchReport();
  resetSearchProgress();
  requireElement<HTMLElement>("#search-progress-panel").hidden = false;
  setStatus("正在搜索", "running");

  try {
    const report = await invoke<SearchReport>("search_folder", {
      rootPath: state.searchRootPath,
      query: requireElement<HTMLInputElement>("#search-query").value,
      includeFiles,
      includeDirectories,
    });

    renderSearchReport(report);
    clearErrorDetails();
    setStatus(report.cancelled ? "搜索已停止" : "搜索完成", "success");
  } catch (error) {
    setErrorStatus(error);
  } finally {
    state.isSearching = false;
    state.isCancellingSearch = false;
    updateSearchButton();
  }
}

async function cancelSearch() {
  if (!state.isSearching || state.isCancellingSearch) {
    return;
  }

  state.isCancellingSearch = true;
  updateSearchButton();
  setStatus("正在停止搜索", "running");

  try {
    await invoke("cancel_search");
  } catch (error) {
    setErrorStatus(error);
  }
}

async function revealSearchResult(path: string) {
  try {
    await invoke("reveal_in_folder", { path });
  } catch (error) {
    setErrorStatus(error);
  }
}

async function deleteSearchResult(path: string) {
  try {
    await invoke("delete_to_trash", { path });
    state.rawSearchResults = state.rawSearchResults.filter(
      (result) => result.path !== path,
    );
    applySearchSort();
    setStatus("已删除到回收站", "success");
  } catch (error) {
    setErrorStatus(error);
  }
}

window.addEventListener("DOMContentLoaded", () => {
  requireElement<HTMLButtonElement>("#merge-tab").addEventListener("click", () =>
    setActiveTab("merge"),
  );
  requireElement<HTMLButtonElement>("#search-tab").addEventListener("click", () =>
    setActiveTab("search"),
  );
  requireElement<HTMLButtonElement>("#select-original").addEventListener(
    "click",
    () => chooseDirectory("original"),
  );
  requireElement<HTMLButtonElement>("#select-merge").addEventListener(
    "click",
    () => chooseDirectory("merge"),
  );
  requireElement<HTMLButtonElement>("#merge-button").addEventListener(
    "click",
    mergeDirectories,
  );
  requireElement<HTMLButtonElement>("#select-search-root").addEventListener(
    "click",
    chooseSearchRootDirectory,
  );
  requireElement<HTMLButtonElement>("#search-button").addEventListener(
    "click",
    searchFolder,
  );
  requireElement<HTMLButtonElement>("#cancel-search-button").addEventListener(
    "click",
    cancelSearch,
  );
  requireElement<HTMLInputElement>("#search-query").addEventListener(
    "keydown",
    (event) => {
      if (event.key === "Enter") {
        searchFolder();
      }
    },
  );
  requireElement<HTMLButtonElement>("#status-detail-button").addEventListener(
    "click",
    openErrorDialog,
  );
  for (const operationInput of document.querySelectorAll<HTMLInputElement>(
    'input[name="operation-mode"]',
  )) {
    operationInput.addEventListener("change", syncOptionsFromControls);
  }
  requireElement<HTMLInputElement>("#archive-duplicates").addEventListener(
    "change",
    syncOptionsFromControls,
  );
  requireElement<HTMLInputElement>("#include-files").addEventListener(
    "change",
    updateSearchButton,
  );
  requireElement<HTMLInputElement>("#include-directories").addEventListener(
    "change",
    updateSearchButton,
  );
  for (const heading of document.querySelectorAll<HTMLButtonElement>(
    "[data-sort-field]",
  )) {
    heading.addEventListener("click", (event) => {
      const field = heading.dataset.sortField as SearchSortField | undefined;

      if (field) {
        toggleSearchSort(field, event.shiftKey);
      }
    });
  }
  listen<MergeProgress>("merge-progress", (event) => {
    scheduleProgressRender(event.payload);
  });
  listen<SearchProgress>("search-progress", (event) => {
    scheduleSearchProgressRender(event.payload);
  });

  setActiveTab("merge");
  syncOptionsFromControls();
  setPath("original", "");
  setPath("merge", "");
  setSearchRootPath("");
  clearSearchReport();
  updateSortHeaders();
  resetProgress();
  resetSearchProgress();
  setStatus("等待选择文件夹", "empty");
});
