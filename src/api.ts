import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import type { ScanProgress, ScanResult, Startup } from "./types";

export const isTauri = typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;

export interface ScanHandle {
  promise: Promise<ScanResult>;
  /** Resolves true when a running scan was asked to stop. */
  cancel: () => void;
}

export function getStartup(): Promise<Startup> {
  return invoke<Startup>("startup");
}

export function listDrives() {
  return invoke<Startup["drives"]>("list_drives");
}

export function scan(root: string, onProgress?: (p: ScanProgress) => void): ScanHandle {
  let cancelled = false;
  const promise = (async () => {
    const unlisten: UnlistenFn | null = onProgress
      ? await listen<ScanProgress>("scan-progress", (event) => {
          if (!cancelled) onProgress(event.payload);
        })
      : null;
    try {
      return await invoke<ScanResult>("scan", { root });
    } finally {
      cancelled = true;
      unlisten?.();
    }
  })();

  return {
    promise,
    cancel: () => {
      cancelled = true;
      void invoke("cancel_scan").catch(() => undefined);
    },
  };
}

export function reveal(path: string): Promise<void> {
  return invoke<void>("reveal", { path });
}
