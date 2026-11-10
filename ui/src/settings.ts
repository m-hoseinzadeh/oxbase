/**
 * App settings the Settings window (workbench/SettingsDialog.tsx) edits, kept in this
 * webview's local storage. Theme, zoom and the table row limit keep their own stores
 * (workbench/theme-bridge, workbench/zoom, workbench/data/rowLimit); the window edits those too.
 */
import { useSyncExternalStore } from "react";
import type { SessionMode } from "./connections/types";

/** Grid row height: compact 24 px, default 28 px, comfortable 34 px. */
export type GridDensity = "compact" | "default" | "comfortable";

export interface Settings {
  gridDensity: GridDensity;
  /** Table data tabs open with Filter & sort shown. */
  filterOpen: boolean;
  /** The Sessions setting new connections start with (where the engine offers it). */
  sessionMode: SessionMode;
  /** Closing the app asks first ("Quit Oxbase?"). */
  confirmQuit: boolean;
}

export const DEFAULT_SETTINGS: Settings = {
  gridDensity: "default",
  filterOpen: false,
  sessionMode: "per_tab",
  confirmQuit: true,
};

export const ROW_HEIGHTS: Record<GridDensity, number> = {
  compact: 24,
  default: 28,
  comfortable: 34,
};

const KEY = "oxbase.settings";
const listeners = new Set<() => void>();
let current: Settings = read();

function read(): Settings {
  try {
    const raw = globalThis.localStorage?.getItem(KEY);
    if (!raw) return DEFAULT_SETTINGS;
    const stored = JSON.parse(raw) as Partial<Record<keyof Settings, unknown>>;
    return {
      gridDensity:
        stored.gridDensity === "compact" || stored.gridDensity === "comfortable"
          ? stored.gridDensity
          : "default",
      filterOpen: stored.filterOpen === true,
      sessionMode: stored.sessionMode === "per_run" ? "per_run" : "per_tab",
      confirmQuit: stored.confirmQuit !== false,
    };
  } catch {
    // Unreadable or unavailable storage: the defaults apply.
    return DEFAULT_SETTINGS;
  }
}

export function getSettings(): Settings {
  return current;
}

export function updateSettings(change: Partial<Settings>): void {
  current = { ...current, ...change };
  try {
    globalThis.localStorage?.setItem(KEY, JSON.stringify(current));
  } catch {
    // Not remembered; the change still applies for this run.
  }
  for (const listener of listeners) listener();
}

function subscribe(listener: () => void): () => void {
  listeners.add(listener);
  return () => listeners.delete(listener);
}

/** The settings; re-renders on change. */
export function useSettings(): Settings {
  return useSyncExternalStore(subscribe, getSettings);
}

// --- opening the Settings window (toolbar button, Ctrl+,) ------------------------------------

const openListeners = new Set<() => void>();

/** Asks the app to show the Settings window. */
export function openSettings(): void {
  for (const listener of openListeners) listener();
}

/** Called whenever the Settings window is asked for; returns the unsubscribe. */
export function onOpenSettings(listener: () => void): () => void {
  openListeners.add(listener);
  return () => openListeners.delete(listener);
}
