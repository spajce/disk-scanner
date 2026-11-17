import "./styles.css";
import { Sunburst } from "./chart";
import { getStartup, isTauri, reveal, scan, type ScanHandle } from "./api";
import { demoResult, demoStartup } from "./demo";
import { formatBytes, formatCount, formatDuration, formatPercent, shortenPath } from "./format";
import type { DiskNode, DriveInfo, Metric, ScanResult } from "./types";

const $ = <T extends Element>(selector: string): T => {
  const element = document.querySelector<T>(selector);
  if (!element) throw new Error(`Missing element: ${selector}`);
  return element;
};

const state: {
  root: DiskNode | null;
  trail: DiskNode[];
  metric: Metric;
  scanning: ScanHandle | null;
  result: ScanResult | null;
  drives: DriveInfo[];
  mount: string | null;
} = {
  root: null,
  trail: [],
  metric: "size",
  scanning: null,
  result: null,
  drives: [],
  mount: null,
};

const chartEl = $<SVGSVGElement>("#chart");
const crumbsEl = $<HTMLDivElement>("#crumbs");
const drivesEl = $<HTMLElement>("#drives");
const detailEl = $<HTMLElement>("#detail");
const barsEl = $<HTMLDivElement>("#bars");
const breakdownHint = $<HTMLSpanElement>("#breakdown-hint");
const statusEl = $<HTMLSpanElement>("#status");
const statusRight = $<HTMLSpanElement>("#status-right");
const overlay = $<HTMLDivElement>("#scan-overlay");
const scanRootEl = $<HTMLSpanElement>("#scan-root");
const scanNumbers = $<HTMLDivElement>("#scan-numbers");
const scanBytes = $<HTMLDivElement>("#scan-bytes");
const scanPath = $<HTMLDivElement>("#scan-path");
const emptyEl = $<HTMLDivElement>("#empty");
const tooltip = $<HTMLDivElement>("#tooltip");
const pathInput = $<HTMLInputElement>("#path-input");
const metricToggle = $<HTMLDivElement>("#metric-toggle");
const btnUp = $<HTMLButtonElement>("#btn-up");
const btnRescan = $<HTMLButtonElement>("#btn-rescan");

const sunburst = new Sunburst(chartEl, {
  metric: "size",
  onSelect: (node) => {
    if (node.children.length > 0) enter(node);
  },
  onHover: (node, event) => {
    if (!node) {
      tooltip.hidden = true;
      return;
    }
    tooltip.hidden = false;
    const metricValue = state.metric === "size" ? formatBytes(node.size) : `${formatCount(node.files)} files`;
    const parent = currentFocus();
    const share = state.metric === "size"
      ? formatPercent(node.size, parent?.size ?? 0)
      : formatPercent(node.files, parent?.files ?? 0);
    tooltip.innerHTML =
      `<strong>${escapeHtml(node.name)}</strong>` +
      `<span>${metricValue} · ${share}</span>` +
      (node.path ? `<em>${escapeHtml(shortenPath(node.path, 64))}</em>` : "");
    if (event) positionTooltip(event);
  },
});

function positionTooltip(event: PointerEvent): void {
  const pad = 14;
  const rect = tooltip.getBoundingClientRect();
  let x = event.clientX + pad;
  let y = event.clientY + pad;
  if (x + rect.width > window.innerWidth - 8) x = event.clientX - rect.width - pad;
  if (y + rect.height > window.innerHeight - 8) y = event.clientY - rect.height - pad;
  tooltip.style.transform = `translate3d(${Math.max(8, x)}px, ${Math.max(8, y)}px, 0)`;
}

function escapeHtml(text: string): string {
  return text.replace(/[&<>"']/g, (ch) => {
    switch (ch) {
      case "&": return "&amp;";
      case "<": return "&lt;";
      case ">": return "&gt;";
      case '"': return "&quot;";
      default: return "&#39;";
    }
  });
}

function currentFocus(): DiskNode | null {
  return state.trail.length ? state.trail[state.trail.length - 1] : state.root;
}

function enter(node: DiskNode): void {
  if (!node.children.length) return;
  const focus = currentFocus();
  if (focus && focus.path === node.path) return;
  state.trail.push(node);
  paintFocus(true);
}

function up(): void {
  if (state.trail.length <= 1) {
    state.trail = [];
    paintFocus(true);
    return;
  }
  state.trail.pop();
  paintFocus(true);
}

function jumpTo(index: number): void {
  state.trail = index < 0 ? [] : state.trail.slice(0, index + 1);
  paintFocus(true);
}

function paintFocus(animate: boolean): void {
  const focus = currentFocus();
  if (!focus) return;
  sunburst.setRoot(focus, animate);
  renderCrumbs();
  renderDetail();
  renderBars();
  btnUp.disabled = state.trail.length === 0;
  btnUp.classList.toggle("muted", state.trail.length === 0);
}

function renderCrumbs(): void {
  const focus = currentFocus();
  if (!focus) {
    crumbsEl.textContent = "";
    return;
  }
  const trail = state.trail.length ? state.trail : [focus];
  const items = trail.map((node, index) => ({ node, index }));
  const shown = items.length > 5 ? items.slice(items.length - 5) : items;

  crumbsEl.innerHTML = "";
  if (items.length > shown.length) {
    const ellipsis = document.createElement("span");
    ellipsis.className = "crumb-dots";
    ellipsis.textContent = "…";
    crumbsEl.append(ellipsis);
  }
  shown.forEach(({ node, index }, position) => {
    const button = document.createElement("button");
    button.type = "button";
    button.className = "crumb";
    button.textContent = node.name || node.path;
    if (index === items.length - 1) button.classList.add("current");
    button.addEventListener("click", () => jumpTo(index));
    crumbsEl.append(button);
    if (position < shown.length - 1) {
      const sep = document.createElement("span");
      sep.className = "crumb-sep";
      sep.textContent = "›";
      crumbsEl.append(sep);
    }
  });
}

function renderDetail(): void {
  const focus = currentFocus();
  if (!focus) {
    detailEl.innerHTML = "";
    return;
  }
  const mount = state.mount;
  const mountSize = state.drives.find((drive) => drive.root === mount)?.total ?? 0;
  const rows: Array<[string, string]> = [
    ["Size", formatBytes(focus.size)],
    ["Files", formatCount(focus.files)],
    ["Folders", formatCount(focus.dirs)],
  ];
  if (mountSize) rows.push(["Of volume", formatPercent(focus.size, mountSize)]);

  const title = focus.name || focus.path || "Aggregated slice";
  const subtitle = focus.path && focus.path !== focus.name
    ? `<p class="detail-path" title="${escapeHtml(focus.path)}">${escapeHtml(shortenPath(focus.path, 54))}</p>`
    : "";
  detailEl.innerHTML = `
    <div class="panel-title">
      <h2>${escapeHtml(focus.kind === "dir" ? "Folder" : "Selection")}</h2>
      <span class="chip">${escapeHtml(focus.kind)}</span>
    </div>
    <p class="detail-name">${escapeHtml(title)}</p>
    ${subtitle}
    <dl class="stats">
      ${rows.map(([label, value]) => `<div><dt>${label}</dt><dd>${value}</dd></div>`).join("")}
    </dl>
  `;

  if (focus.path) {
    const button = document.createElement("button");
    button.type = "button";
    button.className = "btn ghost small";
    button.textContent = "Reveal in Explorer";
    button.addEventListener("click", () => {
      void reveal(focus.path).catch(() => undefined);
    });
    detailEl.append(button);
  }
}

function renderBars(): void {
  const focus = currentFocus();
  barsEl.innerHTML = "";
  if (!focus) {
    breakdownHint.textContent = "";
    return;
  }
  const metric = state.metric;
  const valueOf = (node: DiskNode) => (metric === "size" ? node.size : node.files);
  const children = [...focus.children].sort((a, b) => valueOf(b) - valueOf(a)).slice(0, 14);
  const max = Math.max(1, ...children.map(valueOf));
  breakdownHint.textContent = `${children.length} of ${focus.children.length} shown`;

  children.forEach((child) => {
    const row = document.createElement("button");
    row.type = "button";
    row.className = "bar";
    row.style.setProperty("--pct", `${Math.max(1.5, (valueOf(child) / max) * 100)}%`);
    row.innerHTML = `
      <span class="bar-name">${escapeHtml(child.name)}</span>
      <span class="bar-value">${metric === "size" ? formatBytes(child.size) : formatCount(child.files)}</span>
      <span class="bar-track"><i></i></span>
      <span class="bar-share">${formatPercent(valueOf(child), valueOf(focus))}</span>
    `;
    row.addEventListener("click", () => enter(child));
    row.addEventListener("pointerenter", () => sunburst.highlight(child.path || null));
    row.addEventListener("pointerleave", () => sunburst.highlight(null));
    barsEl.append(row);
  });
}

function renderDrives(activeRoot: string | null): void {
  drivesEl.innerHTML = "";
  for (const drive of state.drives) {
    const button = document.createElement("button");
    button.type = "button";
    button.className = "drive";
    const used = Math.max(0, drive.total - drive.free);
    const pct = drive.total ? (used / drive.total) * 100 : 0;
    button.innerHTML = `
      <span class="drive-head">
        <b>${escapeHtml(drive.root.replace(/\\$/, ""))}</b>
        <span>${formatPercent(used, drive.total, 0)}</span>
      </span>
      <span class="drive-label">${escapeHtml(drive.label || drive.kind)}</span>
      <span class="drive-meter"><i style="width:${pct.toFixed(1)}%"></i></span>
      <span class="drive-free">${formatBytes(drive.free)} free of ${formatBytes(drive.total, 0)}</span>
    `;
    button.classList.toggle("active", activeRoot === drive.root);
    button.addEventListener("click", () => {
      state.mount = drive.root;
      void startScan(drive.root);
    });
    drivesEl.append(button);
  }
}

async function boot(): Promise<void> {
  document.body.classList.add("booting");
  try {
    if (!isTauri) {
      const demo = demoStartup();
      state.drives = demo.drives;
      state.mount = demo.initialRoot;
      renderDrives(demo.initialRoot);
      applyResult(demoResult(demo.initialRoot));
      document.body.classList.remove("booting");
      return;
    }
    const startup = await getStartup();
    state.drives = startup.drives;
    state.mount = startup.initialRoot;
    renderDrives(startup.initialRoot);
    await startScan(startup.initialRoot);
  } catch (error) {
    statusEl.textContent = `Failed to start: ${describe(error)}`;
    showEmpty();
  } finally {
    document.body.classList.remove("booting");
  }
}

function describe(error: unknown): string {
  if (typeof error === "string") return error;
  if (error instanceof Error) return error.message;
  return "unknown error";
}

async function startScan(root: string): Promise<void> {
  if (state.scanning) {
    // Ask the running scan to stop and wait for the backend to release its
    // "already running" guard before starting the next one.
    const previous = state.scanning;
    state.scanning = null;
    previous.cancel();
    await previous.promise.catch(() => undefined);
  }
  state.mount = root;
  overlay.hidden = false;
  emptyEl.hidden = true;
  scanRootEl.textContent = root;
  scanNumbers.textContent = "0 files";
  scanBytes.textContent = "0 B";
  scanPath.textContent = "";
  statusEl.textContent = `Scanning ${root}…`;
  const started = performance.now();

  const handle = scan(root, (progress) => {
    scanNumbers.textContent = `${formatCount(progress.files)} files · ${formatCount(progress.dirs)} folders`;
    scanBytes.textContent = formatBytes(progress.bytes);
    scanPath.textContent = shortenPath(progress.current, 58);
    statusRight.textContent = formatDuration(progress.elapsedMs);
  });
  state.scanning = handle;

  renderDrives(root);

  try {
    const result = await handle.promise;
    if (state.scanning !== handle) return;
    state.scanning = null;
    overlay.hidden = true;
    applyResult(result);
  } catch (error) {
    if (state.scanning !== handle) return;
    state.scanning = null;
    overlay.hidden = true;
    statusEl.textContent = `Scan failed: ${describe(error)}`;
    if (!state.root) showEmpty();
  } finally {
    if (state.scanning === null) {
      statusRight.textContent = formatDuration(performance.now() - started);
    }
  }
}

function applyResult(result: ScanResult): void {
  state.root = result.root;
  state.result = result;
  state.trail = [];
  paintFocus(false);
  btnUp.disabled = true;

  const errorNote = result.errors ? ` · ${formatCount(result.errors)} skipped` : "";
  statusEl.textContent = result.cancelled
    ? `Scan cancelled after ${formatDuration(result.elapsedMs)}`
    : `Scanned ${result.root.path} in ${formatDuration(result.elapsedMs)}${errorNote}`;
  statusRight.textContent =
    `${formatBytes(result.totalSize)} · ${formatCount(result.totalFiles)} files · ` +
    `${formatCount(result.totalDirs)} folders`;
}

function showEmpty(): void {
  overlay.hidden = true;
  emptyEl.hidden = false;
  state.root = null;
  state.trail = [];
  sunburst.setRoot(emptyNode(), false);
  detailEl.innerHTML = "";
  barsEl.innerHTML = "";
  crumbsEl.textContent = "";
}

function emptyNode(): DiskNode {
  return {
    name: "Nothing scanned",
    path: "",
    size: 0,
    files: 0,
    dirs: 0,
    kind: "dir",
    children: [],
  };
}

function setMetric(metric: Metric): void {
  if (state.metric === metric) return;
  state.metric = metric;
  for (const button of metricToggle.querySelectorAll<HTMLButtonElement>("button")) {
    button.classList.toggle("active", button.dataset.metric === metric);
  }
  sunburst.setMetric(metric);
  renderBars();
}

// ---- wiring ---------------------------------------------------------------

metricToggle.addEventListener("click", (event) => {
  const button = (event.target as HTMLElement).closest<HTMLButtonElement>("button[data-metric]");
  if (button?.dataset.metric === "size" || button?.dataset.metric === "files") {
    setMetric(button.dataset.metric);
  }
});

btnUp.addEventListener("click", up);
btnRescan.addEventListener("click", () => {
  if (state.mount) void startScan(state.mount);
});

$<HTMLButtonElement>("#btn-cancel").addEventListener("click", () => {
  state.scanning?.cancel();
  statusEl.textContent = "Cancelling…";
});

$<HTMLButtonElement>("#btn-scan-path").addEventListener("click", () => {
  const value = pathInput.value.trim();
  if (value) void startScan(value);
});
pathInput.addEventListener("keydown", (event) => {
  if (event.key === "Enter") {
    const value = pathInput.value.trim();
    if (value) void startScan(value);
  }
});

window.addEventListener("keydown", (event) => {
  if (event.target instanceof HTMLInputElement) return;
  if (event.key === "Backspace" || event.key === "ArrowUp") {
    event.preventDefault();
    up();
  }
  if (event.key === "Escape") tooltip.hidden = true;
});

window.addEventListener("pointermove", (event) => {
  if (!tooltip.hidden) positionTooltip(event);
});

void boot();
