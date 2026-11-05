/**
 * App-wide keyboard shortcuts. They act on the active tab wherever the focus is in it (the
 * editor, the result grid, a toolbar button), not only while an editor has the focus.
 *
 * | Keys                | Query tab                                        | Table data tab |
 * |---------------------|--------------------------------------------------|----------------|
 * | F5, Ctrl+R          | run the selection, else the whole script         | reload rows    |
 * | Ctrl+Enter          | run the selection, else the statement at cursor  |                |
 * | Ctrl+Shift+Enter    | run the whole script                             |                |
 * | Shift+F5            | stop the running query                           |                |
 * | Ctrl+S              |                                                  | apply changes  |
 *
 * Ctrl+= (Ctrl++), Ctrl+- and Ctrl+0 zoom the whole app in, out and back to 100 % (./zoom),
 * whichever tab is active.
 *
 * The webview's own reload keys (F5, Ctrl+R, Ctrl+Shift+R, Ctrl+F5, Shift+F5) never reload the
 * app, which would drop every open session: they are always cancelled. A key a focused
 * component already handled (`defaultPrevented`: the object tree's F5 refresh, Monaco's own
 * Ctrl+Enter) is left alone.
 */

export type ShortcutCommand = "run" | "runCurrent" | "runAll" | "cancel" | "save";

import { resetZoom, zoomIn, zoomOut } from "./zoom";

/** Runs `command` for its tab; `false` when the tab has nothing for it. */
export type ShortcutHandler = (command: ShortcutCommand) => boolean;

const handlers = new Map<string, ShortcutHandler>();

/** Registers the shortcuts of the tab (dockview panel) `panelId`; returns the unregister. */
export function registerShortcuts(panelId: string, handler: ShortcutHandler): () => void {
  handlers.set(panelId, handler);
  return () => {
    if (handlers.get(panelId) === handler) handlers.delete(panelId);
  };
}

interface Keys {
  key: string;
  /** The physical key ("Equal", "Minus", "Digit0", ...), for layouts where `key` differs. */
  code?: string;
  ctrlKey: boolean;
  metaKey: boolean;
  shiftKey: boolean;
  altKey: boolean;
}

/** What a key press means: a command, a reload key to cancel, or nothing. */
export function shortcutOf(event: Keys): ShortcutCommand | "reload" | null {
  const mod = event.ctrlKey || event.metaKey;
  if (event.altKey) return null;
  const key = event.key.length === 1 ? event.key.toLowerCase() : event.key;
  if (key === "F5") {
    if (mod) return "reload";
    return event.shiftKey ? "cancel" : "run";
  }
  if (mod && key === "r") return event.shiftKey ? "reload" : "run";
  if (mod && key === "Enter") return event.shiftKey ? "runAll" : "runCurrent";
  if (mod && !event.shiftKey && key === "s") return "save";
  return null;
}

export type ZoomCommand = "zoomIn" | "zoomOut" | "zoomReset";

/** Whether a key press zooms the app: Ctrl with =/+, -, or 0 (main row or keypad). */
export function zoomShortcutOf(event: Keys): ZoomCommand | null {
  if (!(event.ctrlKey || event.metaKey) || event.altKey) return null;
  const { key, code = "" } = event;
  if (key === "=" || key === "+" || code === "Equal" || code === "NumpadAdd") return "zoomIn";
  if (key === "-" || key === "_" || code === "Minus" || code === "NumpadSubtract") {
    return "zoomOut";
  }
  if (key === "0" || code === "Digit0" || code === "Numpad0") return "zoomReset";
  return null;
}

const ZOOM: Record<ZoomCommand, () => void> = {
  zoomIn,
  zoomOut,
  zoomReset: resetZoom,
};

/** The keys a command reads as in a tooltip, e.g. "F5". */
export const SHORTCUT_LABELS: Record<ShortcutCommand, string> = {
  run: "F5",
  runCurrent: "Ctrl+Enter",
  runAll: "Ctrl+Shift+Enter",
  cancel: "Shift+F5",
  save: "Ctrl+S",
};

/**
 * Installs the shortcuts on `target` (the window); `activePanel` names the active tab. Returns
 * a function that removes them.
 */
export function installShortcuts(
  activePanel: () => string | null | undefined,
  target: Window = window,
): () => void {
  const onKeyDown = (event: KeyboardEvent) => {
    if (event.defaultPrevented) return;
    const zoom = zoomShortcutOf(event);
    if (zoom !== null) {
      // Held keys keep zooming, as in browsers.
      event.preventDefault();
      ZOOM[zoom]();
      return;
    }
    if (event.repeat) return;
    const shortcut = shortcutOf(event);
    if (shortcut === null) return;
    const reloads = event.key === "F5" || event.key.toLowerCase() === "r";
    if (shortcut !== "reload") {
      const id = activePanel();
      const handler = id ? handlers.get(id) : undefined;
      if (handler?.(shortcut)) {
        event.preventDefault();
        return;
      }
    }
    // A reload key nothing used: still never reload the app.
    if (reloads) event.preventDefault();
  };
  target.addEventListener("keydown", onKeyDown);
  return () => target.removeEventListener("keydown", onKeyDown);
}
