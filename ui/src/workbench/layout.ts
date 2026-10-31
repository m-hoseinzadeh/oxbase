/**
 * Opens workbench panels from anywhere (tree, history, toolbar): the dockview API is registered
 * here by the app shell once the layout is ready.
 */
import type { DockviewApi } from "dockview-react";

/** What a new query editor starts with (`params` of its dockview panel). */
export interface QueryEditorParams {
  editorId: string;
  /** Base session to run on; the editor offers the open sessions when absent. */
  sessionId?: string;
  /** Database to run in (RunOptions.database). */
  database?: string;
  /** Initial SQL text. */
  sql?: string;
}

export type QueryEditorInit = Omit<QueryEditorParams, "editorId">;

let dockview: DockviewApi | null = null;
let counter = 0;

export function setDockviewApi(api: DockviewApi | null): void {
  dockview = api;
}

/** A fresh editor id (1 to 200 bytes, unique for this app run). */
export function newEditorId(): string {
  counter += 1;
  return `editor-${Date.now().toString(36)}-${counter}`;
}

/** Opens a new query editor tab next to the other editors. Returns its editor id. */
export function openQueryEditor(init: QueryEditorInit = {}): string | null {
  const api = dockview;
  if (!api) return null;
  const editorId = newEditorId();
  const params: QueryEditorParams = { editorId, ...init };
  const neighbour = api.panels.find((p) => p.id === "welcome" || p.id.startsWith("editor-"));
  api.addPanel({
    id: editorId,
    component: "query",
    title: `Query ${counter}`,
    params,
    ...(neighbour ? { position: { referencePanel: neighbour.id, direction: "within" } } : {}),
  });
  return editorId;
}

/** Shows (or re-adds) the History panel. */
export function showHistory(): void {
  const api = dockview;
  if (!api) return;
  const existing = api.getPanel("history");
  if (existing) {
    existing.api.setActive();
    return;
  }
  api.addPanel({
    id: "history",
    component: "history",
    title: "History",
    position: { direction: "below" },
    initialHeight: 220,
  });
}
