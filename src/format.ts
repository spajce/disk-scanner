const UNITS = ["B", "KB", "MB", "GB", "TB", "PB"];

export function formatBytes(bytes: number, digits = 1): string {
  if (!Number.isFinite(bytes) || bytes <= 0) return "0 B";
  const exponent = Math.min(UNITS.length - 1, Math.floor(Math.log(bytes) / Math.log(1024)));
  const value = bytes / 1024 ** exponent;
  if (exponent === 0) return `${Math.round(value)} B`;
  if (value >= 100) return `${value.toFixed(0)} ${UNITS[exponent]}`;
  return `${value.toFixed(digits)} ${UNITS[exponent]}`;
}

export function formatCount(count: number): string {
  return Math.round(count).toLocaleString("en-US");
}

export function formatPercent(part: number, whole: number, digits = 1): string {
  if (!whole || !Number.isFinite(part)) return "0%";
  const percent = (part / whole) * 100;
  if (percent >= 99.95) return "100%";
  if (percent >= 10) return `${percent.toFixed(0)}%`;
  if (percent >= 1) return `${percent.toFixed(digits)}%`;
  if (percent >= 0.01) return `${percent.toFixed(2)}%`;
  return "<0.01%";
}

export function formatDuration(ms: number): string {
  if (!Number.isFinite(ms) || ms < 0) return "—";
  if (ms < 1000) return `${Math.round(ms)} ms`;
  const seconds = ms / 1000;
  if (seconds < 60) return `${seconds.toFixed(seconds < 10 ? 1 : 0)} s`;
  const minutes = Math.floor(seconds / 60);
  return `${minutes}m ${Math.round(seconds % 60)}s`;
}

/** `C:\Users\jcarillo\Documents` -> `…\jcarillo\Documents` */
export function shortenPath(path: string, max = 46): string {
  if (path.length <= max) return path;
  return `…${path.slice(path.length - max + 1)}`;
}
