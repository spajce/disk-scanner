export type NodeKind = "dir" | "file" | "other";

export interface DiskNode {
  name: string;
  /** Absolute path. Empty for aggregated `(n more)` slices. */
  path: string;
  size: number;
  files: number;
  dirs: number;
  kind: NodeKind;
  children: DiskNode[];
}

export interface DriveInfo {
  root: string;
  label: string;
  kind: string;
  total: number;
  free: number;
}

export interface ScanResult {
  root: DiskNode;
  totalSize: number;
  totalFiles: number;
  totalDirs: number;
  errors: number;
  elapsedMs: number;
  cancelled: boolean;
  nodeCount: number;
}

export interface ScanProgress {
  files: number;
  dirs: number;
  bytes: number;
  errors: number;
  current: string;
  elapsedMs: number;
}

export interface Startup {
  drives: DriveInfo[];
  initialRoot: string;
  lastRoot: string | null;
}

export type Metric = "size" | "files";
