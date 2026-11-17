import type { DiskNode, ScanResult, Startup } from "./types";

/** Deterministic browser-only fixture so `pnpm dev` renders without the Rust host. */

function mulberry32(seed: number): () => number {
  let a = seed >>> 0;
  return () => {
    a = (a + 0x6d2b79f5) >>> 0;
    let t = Math.imul(a ^ (a >>> 15), 1 | a);
    t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

const GB = 1024 ** 3;
const MB = 1024 ** 2;

interface Spec {
  name: string;
  size: number;
  children?: string[];
}

const TOP: Spec[] = [
  { name: "Windows", size: 42.6 * GB, children: ["WinSxS", "System32", "SysWOW64", "Installer", "SoftwareDistribution", "servicing", "assembly", "Fonts"] },
  { name: "Users", size: 138.4 * GB, children: ["jcarillo", "Public", "Default"] },
  { name: "Program Files", size: 61.2 * GB, children: ["Microsoft", "Adobe", "Docker", "Mozilla Firefox", "Git", "nodejs", "Common Files"] },
  { name: "Program Files (x86)", size: 18.9 * GB, children: ["Steam", "Microsoft", "Google", "Common Files"] },
  { name: "ProgramData", size: 26.7 * GB, children: ["Microsoft", "Docker", "NVIDIA Corporation", "Package Cache", "Adobe"] },
  { name: "Games", size: 74.2 * GB, children: ["SteamLibrary", "Epic Games", "Battle.net"] },
  { name: "Media", size: 51.8 * GB, children: ["Videos", "Music", "Pictures", "Podcasts"] },
  { name: "Dev", size: 22.4 * GB, children: ["repos", "target", "node_modules", "sdks", "cache"] },
  { name: "Backups", size: 9.6 * GB },
  { name: "pagefile.sys", size: 8 * GB },
  { name: "hiberfil.sys", size: 6.4 * GB },
];

const LEAF_NAMES = [
  "bin", "data", "logs", "cache", "lib", "assets", "docs", "tmp", "plugins", "resources",
  "src", "dist", "packages", "config", "models", "fonts", "samples", "vendor", "obj", "out",
];

function makeChildren(rand: () => number, parent: string, size: number, depth: number, path: string): DiskNode[] {
  if (depth <= 0 || size < 24 * MB) return [];
  const count = depth >= 4 ? 2 + Math.floor(rand() * 3) : 3 + Math.floor(rand() * 6);
  const weights = Array.from({ length: count }, () => 0.25 + rand() * rand() * 2.6);
  const total = weights.reduce((sum, w) => sum + w, 0);
  return weights.map((weight, index) => {
    const share = (weight / total) * size;
    const name = depth === 4 && index === 0 && rand() > 0.55
      ? `setup-${Math.floor(rand() * 9) + 1}.exe`
      : LEAF_NAMES[(Math.floor(rand() * LEAF_NAMES.length) + index) % LEAF_NAMES.length];
    const childPath = `${path}\\${name}`;
    const isFile = name.endsWith(".exe");
    const children = isFile ? [] : makeChildren(rand, name, share, depth - 1, childPath);
    const files = isFile
      ? 1
      : Math.max(1, Math.round(share / (180 * 1024 + rand() * 420 * 1024)));
    return {
      name,
      path: childPath,
      size: Math.round(share),
      files,
      dirs: children.length,
      kind: isFile ? ("file" as const) : ("dir" as const),
      children,
    };
  });
}

export function demoStartup(): Startup {
  return {
    drives: [
      { root: "C:\\", label: "System", kind: "fixed", total: 476_700_000_000, free: 132_400_000_000 },
      { root: "G:\\", label: "Archive", kind: "fixed", total: 16_200_000_000, free: 13_700_000_000 },
    ],
    initialRoot: "C:\\",
    lastRoot: "C:\\",
  };
}

export function demoResult(root = "C:\\"): ScanResult {
  const rand = mulberry32(20261117);
  const children = TOP.map((spec) => {
    const path = `${root}${spec.name}`;
    const isFile = spec.name.endsWith(".sys");
    const grand = isFile ? [] : makeChildren(rand, spec.name, spec.size, 4, path);
    return {
      name: spec.name,
      path,
      size: Math.round(spec.size),
      files: isFile ? 1 : Math.max(1, Math.round(spec.size / (260 * 1024))),
      dirs: grand.length,
      kind: isFile ? ("file" as const) : ("dir" as const),
      children: grand,
    };
  });

  const totalSize = children.reduce((sum, child) => sum + child.size, 0);
  const totalFiles = children.reduce((sum, child) => sum + child.files, 0);
  const totalDirs = children.reduce((sum, child) => sum + child.dirs, 0);

  return {
    root: {
      name: root,
      path: root,
      size: totalSize,
      files: totalFiles,
      dirs: totalDirs,
      kind: "dir",
      children,
    },
    totalSize,
    totalFiles,
    totalDirs,
    errors: 0,
    elapsedMs: 4820,
    cancelled: false,
    nodeCount: 640,
  };
}
