/**
 * App zoom: Ctrl+= / Ctrl+- / Ctrl+0 (./shortcuts) scale the whole window, the way a browser
 * zooms a page. The level is the webview's zoom factor (`Webview.setZoom`), so text, the grid's
 * canvas and the editors stay sharp. The last level is remembered across runs.
 */
import { isTauri } from "@tauri-apps/api/core";
import { getCurrentWebview } from "@tauri-apps/api/webview";
import { useSyncExternalStore } from "react";

/** The zoom steps, as in browsers. */
export const ZOOM_LEVELS = [0.5, 0.67, 0.75, 0.8, 0.9, 1, 1.1, 1.25, 1.5, 1.75, 2] as const;

const STORAGE_KEY = "oxbase.zoom";
const listeners = new Set<() => void>();
let zoom = readStored();

function readStored(): number {
  try {
    const value = Number(globalThis.localStorage?.getItem(STORAGE_KEY));
    if ((ZOOM_LEVELS as readonly number[]).includes(value)) return value;
  } catch {
    // Storage can be unavailable (private mode, tests): 100 % applies.
  }
  return 1;
}

function applyNative() {
  // Outside the app (tests, a plain browser) there is no webview to zoom.
  if (!isTauri()) return;
  try {
    getCurrentWebview()
      .setZoom(zoom)
      .catch((error: unknown) => console.warn("could not zoom the window", error));
  } catch (error) {
    console.warn("could not zoom the window", error);
  }
}

/** Applies the remembered level at start-up. */
export function initZoom(): void {
  if (zoom !== 1) applyNative();
}

export function getZoom(): number {
  return zoom;
}

export function setZoom(next: number): void {
  if (next === zoom) return;
  zoom = next;
  try {
    globalThis.localStorage?.setItem(STORAGE_KEY, String(next));
  } catch {
    // Not remembered; the level still applies for this run.
  }
  applyNative();
  for (const listener of listeners) listener();
}

/** The next step up (`+1`) or down (`-1`) from `level`, kept within the steps. */
export function zoomStep(level: number, direction: 1 | -1): number {
  const steps = ZOOM_LEVELS as readonly number[];
  if (direction > 0) return steps.find((s) => s > level + 1e-9) ?? steps[steps.length - 1] ?? 1;
  return [...steps].reverse().find((s) => s < level - 1e-9) ?? steps[0] ?? 1;
}

export function zoomIn(): void {
  setZoom(zoomStep(zoom, 1));
}

export function zoomOut(): void {
  setZoom(zoomStep(zoom, -1));
}

export function resetZoom(): void {
  setZoom(1);
}

/** "110%". */
export function zoomLabel(level: number): string {
  return `${Math.round(level * 100)}%`;
}

function subscribe(listener: () => void): () => void {
  listeners.add(listener);
  return () => listeners.delete(listener);
}

/** The current level; re-renders on change. */
export function useZoom(): number {
  return useSyncExternalStore(subscribe, getZoom);
}
