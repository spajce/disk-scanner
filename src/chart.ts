import type { DiskNode, Metric } from "./types";
import { formatBytes, formatCount } from "./format";

const TAU = Math.PI * 2;
const MAX_RINGS = 5;
const PAD_PX = 1.6;
const EASE_OUT_CUBIC = (t: number) => 1 - (1 - t) ** 3;

export interface Slice {
  key: string;
  node: DiskNode;
  depth: number;
  parentKey: string | null;
  x0: number;
  x1: number;
}

interface Angles {
  x0: number;
  x1: number;
}

export interface SunburstOptions {
  metric: Metric;
  onSelect?: (node: DiskNode, event: MouseEvent) => void;
  onHover?: (node: DiskNode | null, event?: PointerEvent) => void;
}

function valueOf(node: DiskNode, metric: Metric): number {
  if (metric === "files") return Math.max(0, node.files || 0);
  return Math.max(0, node.size || 0);
}

const PALETTE = [206, 188, 168, 146, 118, 92, 66, 42, 24, 8, 342, 320, 288, 262];

/** Children inherit a nearby hue from their parent; roots walk a fixed palette. */
function childHue(parentHue: number | null, index: number): number {
  if (parentHue === null) return PALETTE[index % PALETTE.length];
  const offsets = [-38, 0, 38, -19, 19, -54, 54];
  return (parentHue + offsets[index % offsets.length] + 360) % 360;
}

function hashHue(text: string): number {
  let hash = 2166136261;
  for (let i = 0; i < text.length; i += 1) {
    hash ^= text.charCodeAt(i);
    hash = Math.imul(hash, 16777619);
  }
  return Math.abs(hash) % 360;
}

function polar(cx: number, cy: number, radius: number, angle: number): [number, number] {
  return [cx + Math.cos(angle) * radius, cy + Math.sin(angle) * radius];
}

function arcPath(
  cx: number,
  cy: number,
  inner: number,
  outer: number,
  a0: number,
  a1: number,
): string {
  const span = a1 - a0;
  const large = span > Math.PI ? 1 : 0;
  const [ox0, oy0] = polar(cx, cy, outer, a0);
  const [ox1, oy1] = polar(cx, cy, outer, a1);
  const [ix1, iy1] = polar(cx, cy, inner, a1);
  const [ix0, iy0] = polar(cx, cy, inner, a0);
  if (inner <= 0.5) {
    return `M ${cx} ${cy} L ${ox0} ${oy0} A ${outer} ${outer} 0 ${large} 1 ${ox1} ${oy1} Z`;
  }
  return (
    `M ${ox0} ${oy0} A ${outer} ${outer} 0 ${large} 1 ${ox1} ${oy1} ` +
    `L ${ix1} ${iy1} A ${inner} ${inner} 0 ${large} 0 ${ix0} ${iy0} Z`
  );
}

const SVG_NS = "http://www.w3.org/2000/svg";

export class Sunburst {
  private readonly svg: SVGSVGElement;
  private readonly arcLayer: SVGGElement;
  private readonly labelLayer: SVGGElement;
  private readonly hubLayer: SVGGElement;
  private readonly hubValue: SVGTextElement;
  private readonly hubName: SVGTextElement;
  private readonly hubCaption: SVGTextElement;
  private readonly opts: SunburstOptions;

  private focusNode: DiskNode | null = null;
  private slices: Slice[] = [];
  private paths = new Map<string, SVGPathElement>();
  private labels = new Map<string, SVGTextElement>();
  private ancestors = new Map<string, string[]>();
  private renderedKeys = "";

  private width = 0;
  private height = 0;
  private innerRadius = 0;
  private ringWidth = 1;

  private current = new Map<string, Angles>();
  private from = new Map<string, Angles>();
  private animStart = 0;
  private frame = 0;

  private hoverKey: string | null = null;

  constructor(host: SVGSVGElement, opts: SunburstOptions) {
    this.svg = host;
    this.opts = opts;

    this.arcLayer = document.createElementNS(SVG_NS, "g");
    this.arcLayer.setAttribute("class", "arc-layer");
    this.labelLayer = document.createElementNS(SVG_NS, "g");
    this.labelLayer.setAttribute("class", "label-layer");
    this.hubLayer = document.createElementNS(SVG_NS, "g");
    this.hubLayer.setAttribute("class", "hub-layer");

    const hub = document.createElementNS(SVG_NS, "circle");
    hub.setAttribute("class", "hub");
    this.hubName = document.createElementNS(SVG_NS, "text");
    this.hubName.setAttribute("class", "hub-name");
    this.hubValue = document.createElementNS(SVG_NS, "text");
    this.hubValue.setAttribute("class", "hub-value");
    this.hubCaption = document.createElementNS(SVG_NS, "text");
    this.hubCaption.setAttribute("class", "hub-caption");
    this.hubLayer.append(hub, this.hubName, this.hubValue, this.hubCaption);

    this.svg.append(this.arcLayer, this.labelLayer, this.hubLayer);

    this.arcLayer.addEventListener("pointerover", this.handlePointerOver);
    this.arcLayer.addEventListener("pointerout", this.handlePointerOut);
    this.arcLayer.addEventListener("pointermove", this.handlePointerMove);
    this.arcLayer.addEventListener("click", this.handleClick);

    const observer = new ResizeObserver(() => this.measure());
    observer.observe(this.svg);
    this.measure();
  }

  get focus(): DiskNode | null {
    return this.focusNode;
  }

  setMetric(metric: Metric): void {
    if (this.opts.metric === metric) return;
    this.opts.metric = metric;
    this.rebuild(true);
  }

  setRoot(node: DiskNode, animate = true): void {
    const same = this.focusNode?.path && this.focusNode.path === node.path;
    this.focusNode = node;
    if (same) {
      this.rebuild(false);
      return;
    }
    this.rebuild(animate);
  }

  highlight(key: string | null): void {
    if (this.hoverKey === key) return;
    this.hoverKey = key;
    this.applyHighlight();
  }

  /** Key used for a node (mirrors the layout key scheme). */
  static keyOf(node: DiskNode, parentKey: string | null): string {
    if (node.path) return node.path;
    return `${parentKey ?? "root"}|${node.name}`;
  }

  private measure(): void {
    const rect = this.svg.getBoundingClientRect();
    const width = Math.max(240, Math.round(rect.width));
    const height = Math.max(240, Math.round(rect.height));
    if (width === this.width && height === this.height) return;
    this.width = width;
    this.height = height;
    this.svg.setAttribute("viewBox", `0 0 ${width} ${height}`);
    this.updateGeometry();
    this.renderNow();
  }

  /**
   * Ring geometry depends on both the available box and how deep the focused
   * node goes, so it has to be recomputed on every rebuild — computing it only
   * in `measure()` leaves the first render with a one-ring layout (a single
   * giant band) because the focus is still empty at that point.
   */
  private updateGeometry(): void {
    const radius = Math.min(this.width, this.height) / 2 - 6;
    this.innerRadius = radius * 0.44;
    const rings = Math.min(MAX_RINGS, Math.max(1, this.maxDepth(this.focusNode)));
    this.ringWidth = (radius - this.innerRadius) / rings;
  }

  private maxDepth(node: DiskNode | null): number {
    if (!node) return 1;
    let depth = 0;
    const walk = (n: DiskNode, d: number) => {
      depth = Math.max(depth, d);
      if (d >= MAX_RINGS) return;
      for (const child of n.children) walk(child, d + 1);
    };
    walk(node, 0);
    return depth;
  }

  private layout(focus: DiskNode): Slice[] {
    const slices: Slice[] = [];
    const ancestors = new Map<string, string[]>();
    const rootKey = "root";

    const walk = (
      node: DiskNode,
      key: string,
      parentKey: string | null,
      depth: number,
      a0: number,
      a1: number,
      chain: string[],
      parentHue: number | null,
    ) => {
      slices.push({ key, node, depth, parentKey, x0: a0, x1: a1 });
      ancestors.set(key, chain);
      if (depth >= MAX_RINGS || node.children.length === 0) return;

      const kids = node.children
        .map((child) => ({ child, value: valueOf(child, this.opts.metric) }))
        .filter((entry) => entry.value > 0);
      if (kids.length === 0) return;

      const total = kids.reduce((sum, entry) => sum + entry.value, 0);
      const span = a1 - a0;
      let cursor = a0;
      kids.forEach((entry, index) => {
        const width = (entry.value / total) * span;
        const childKey = Sunburst.keyOf(entry.child, key);
        const hue = childHue(parentHue, index);
        this.hueCache.set(childKey, hue);
        walk(entry.child, childKey, key, depth + 1, cursor, cursor + width, [...chain, key], hue);
        cursor += width;
      });
    };

    walk(focus, rootKey, null, 0, -Math.PI / 2, -Math.PI / 2 + TAU, [], null);
    this.ancestors = ancestors;
    return slices;
  }

  private readonly hueCache = new Map<string, number>();

  private rebuild(animate: boolean): void {
    const focus = this.focusNode;
    if (!focus || this.width === 0) return;

    const previous = new Map(this.current);
    this.slices = this.layout(focus).filter((slice) => slice.depth > 0);
    this.updateGeometry();

    const keys = this.slices.map((slice) => slice.key).join("|");
    const structureChanged = keys !== this.renderedKeys;

    if (structureChanged) {
      this.renderedKeys = keys;
      this.syncDom();
    }

    this.from = previous;
    this.animStart = performance.now();
    if (!animate) {
      this.current = new Map(this.slices.map((s) => [s.key, { x0: s.x0, x1: s.x1 }]));
      this.renderNow();
      return;
    }
    this.startLoop();
  }

  private startLoop(): void {
    if (this.frame) return;
    const tick = () => {
      const duration = 520;
      const t = Math.min(1, (performance.now() - this.animStart) / duration);
      const eased = EASE_OUT_CUBIC(t);
      this.current.clear();
      for (const slice of this.slices) {
        const target = { x0: slice.x0, x1: slice.x1 };
        const start = this.from.get(slice.key);
        if (!start) {
          this.current.set(slice.key, { x0: target.x0, x1: target.x0 });
          // New slices grow out of their parent's starting edge.
          const parentStart = slice.parentKey ? this.from.get(slice.parentKey) : undefined;
          const anchor = parentStart ? parentStart.x0 : target.x0;
          this.current.set(slice.key, { x0: anchor, x1: anchor + (target.x1 - target.x0) * eased });
          continue;
        }
        this.current.set(slice.key, {
          x0: start.x0 + (target.x0 - start.x0) * eased,
          x1: start.x1 + (target.x1 - start.x1) * eased,
        });
      }
      this.renderNow();
      if (t < 1) {
        this.frame = requestAnimationFrame(tick);
      } else {
        this.frame = 0;
        this.current = new Map(this.slices.map((s) => [s.key, { x0: s.x0, x1: s.x1 }]));
        this.renderNow();
      }
    };
    this.frame = requestAnimationFrame(tick);
  }

  private syncDom(): void {
    for (const [key, path] of this.paths) {
      if (!this.slices.some((slice) => slice.key === key)) {
        path.remove();
        this.paths.delete(key);
        this.labels.get(key)?.remove();
        this.labels.delete(key);
      }
    }

    this.slices.forEach((slice, index) => {
      let path = this.paths.get(slice.key);
      if (!path) {
        path = document.createElementNS(SVG_NS, "path");
        path.setAttribute("class", "slice");
        path.setAttribute("data-key", slice.key);
        this.paths.set(slice.key, path);
      }
      path.setAttribute("data-depth", String(Math.min(slice.depth, 6)));
      const fill = slice.node.kind === "other" ? "hsl(220 8% 46%)" : this.colorFor(slice.key, slice.depth, index);
      path.style.setProperty("--fill", fill);
      if (path.parentNode !== this.arcLayer) this.arcLayer.append(path);
    });

    // Keep DOM order aligned with layout order (parents first, children on top).
    for (const slice of this.slices) {
      const path = this.paths.get(slice.key);
      if (path && path.nextSibling !== null) this.arcLayer.append(path);
    }
    this.applyHighlight();
  }

  private colorFor(key: string, depth: number, index: number): string {
    const hue = this.hueCache.get(key) ?? (hashHue(key) + index * 23) % 360;
    const light = Math.max(30, 62 - (depth - 1) * 6);
    const sat = Math.max(26, 46 - (depth - 1) * 3);
    return `hsl(${hue} ${sat}% ${light}%)`;
  }

  private padRadians(radius: number): number {
    return Math.min(PAD_PX / Math.max(radius, 1), 0.08);
  }

  private renderNow(): void {
    const cx = this.width / 2;
    const cy = this.height / 2;

    for (const slice of this.slices) {
      const path = this.paths.get(slice.key);
      if (!path) continue;
      const angle = this.current.get(slice.key) ?? { x0: slice.x0, x1: slice.x1 };
      const inner = this.innerRadius + (slice.depth - 1) * this.ringWidth;
      const outer = inner + this.ringWidth - Math.min(2.5, this.ringWidth * 0.12);
      const pad = this.padRadians(outer);
      let a0 = angle.x0 + pad;
      let a1 = angle.x1 - pad;
      if (a1 - a0 < 0.0009) {
        a0 = angle.x0;
        a1 = Math.max(angle.x1, angle.x0 + 0.0009);
      }
      path.setAttribute("d", arcPath(cx, cy, inner, outer, a0, a1));
      const hasRoom = (angle.x1 - angle.x0) * outer > 3;
      path.style.opacity = hasRoom ? "1" : "0";
    }

    this.renderLabels(cx, cy);
    this.renderHub(cx, cy);
  }

  private renderLabels(cx: number, cy: number): void {
    const placeholders = new Map(this.labels);
    for (const slice of this.slices) {
      const angle = this.current.get(slice.key) ?? { x0: slice.x0, x1: slice.x1 };
      const inner = this.innerRadius + (slice.depth - 1) * this.ringWidth;
      const outer = inner + this.ringWidth;
      const mid = (angle.x0 + angle.x1) / 2;
      const arcLength = (angle.x1 - angle.x0) * ((inner + outer) / 2);
      const minArc = Math.max(30, slice.node.name.length * 5.4);
      const show = slice.node.name && arcLength > minArc && this.ringWidth > 17 && slice.depth <= 3;
      if (!show) continue;

      let text = placeholders.get(slice.key);
      if (text) placeholders.delete(slice.key);
      if (!text) {
        text = document.createElementNS(SVG_NS, "text");
        text.setAttribute("class", "slice-label");
        this.labels.set(slice.key, text);
      }
      const radius = (inner + outer) / 2 + 1;
      const [x, y] = polar(cx, cy, radius, mid);
      let degrees = (mid * 180) / Math.PI;
      if (degrees > 90 || degrees < -90) degrees += 180;
      text.setAttribute("transform", `rotate(${degrees.toFixed(2)} ${x.toFixed(2)} ${y.toFixed(2)})`);
      text.setAttribute("x", x.toFixed(2));
      text.setAttribute("y", y.toFixed(2));
      text.textContent = slice.node.name;
      if (text.parentNode !== this.labelLayer) this.labelLayer.append(text);
    }
    for (const leftover of placeholders.values()) leftover.remove();
  }

  private renderHub(cx: number, cy: number): void {
    const node = this.focusNode;
    const hub = this.hubLayer.querySelector("circle.hub");
    hub?.setAttribute("cx", String(cx));
    hub?.setAttribute("cy", String(cy));
    hub?.setAttribute("r", String(this.innerRadius - 1));

    const metric = this.opts.metric;
    const value = node ? (metric === "size" ? node.size : node.files) : 0;
    const name = node ? (node.path || node.name) : "";
    const caption = metric === "size" ? "bytes" : "files";

    this.hubName.setAttribute("x", String(cx));
    this.hubName.setAttribute("y", String(cy - this.innerRadius * 0.30));
    this.hubName.textContent = shorten(name, Math.round(this.innerRadius * 0.30));

    this.hubValue.setAttribute("x", String(cx));
    this.hubValue.setAttribute("y", String(cy + this.innerRadius * 0.06));
    this.hubValue.style.fontSize = `${Math.max(16, this.innerRadius * 0.26).toFixed(1)}px`;
    this.hubValue.textContent = metric === "size" ? formatBytes(value, 1) : formatCount(value);

    this.hubCaption.setAttribute("x", String(cx));
    this.hubCaption.setAttribute("y", String(cy + this.innerRadius * 0.34));
    this.hubCaption.textContent = caption;
  }

  private applyHighlight(): void {
    const key = this.hoverKey;
    const chain = key ? new Set([key, ...(this.ancestors.get(key) ?? [])]) : null;
    for (const [sliceKey, path] of this.paths) {
      if (!chain) {
        path.classList.remove("on", "dim");
        continue;
      }
      const isOn = chain.has(sliceKey) || (key !== null && sliceKey.startsWith(`${key}|`));
      path.classList.toggle("on", isOn);
      path.classList.toggle("dim", !isOn);
    }
  }

  private sliceFromEvent(event: Event): Slice | null {
    const target = event.target as Element | null;
    const key = target?.getAttribute?.("data-key");
    if (!key) return null;
    return this.slices.find((slice) => slice.key === key) ?? null;
  }

  private handlePointerOver = (event: PointerEvent): void => {
    const slice = this.sliceFromEvent(event);
    if (!slice) return;
    this.hoverKey = slice.key;
    this.applyHighlight();
    this.opts.onHover?.(slice.node, event);
  };

  private handlePointerOut = (event: PointerEvent): void => {
    const related = event.relatedTarget as Element | null;
    if (related && this.arcLayer.contains(related)) return;
    this.hoverKey = null;
    this.applyHighlight();
    this.opts.onHover?.(null);
  };

  private handlePointerMove = (event: PointerEvent): void => {
    if (!this.hoverKey) return;
    this.opts.onHover?.(this.slices.find((s) => s.key === this.hoverKey)?.node ?? null, event);
  };

  private handleClick = (event: MouseEvent): void => {
    const slice = this.sliceFromEvent(event);
    if (!slice) return;
    this.opts.onSelect?.(slice.node, event);
  };
}

function shorten(path: string, max: number): string {
  if (path.length <= max) return path;
  const tail = Math.max(6, Math.floor(max * 0.7));
  return `…${path.slice(-tail)}`;
}
