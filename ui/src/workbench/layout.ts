/**
 * Opens workbench panels from anywhere (tree, history, toolbar): the dockview API is registered
 * here by the app shell once the layout is ready.
 */
import type { DockviewApi } from "dockview-react";
import type { BinaryText, ObjectRef } from "../api";
import { qualifiedName } from "./names";

/** What a new query editor starts with (`params` of its dockview panel). */
export interface QueryEditorParams {
  editorId: string;
  /** Base session to run on; the editor offers the open sessions when absent. */
  sessionId?: string;
  /** Database to run in (RunOptions.database). */
  database?: string;
  /** Initial SQL text. */
  sql?: string;
  /** Tab title (default `Query <n>`; the mongo shell says so). */
  title?: string;
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
    title: init.title ?? `Query ${counter}`,
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

// --- M3: table data and table designer tabs -----------------------------------------------

/** `params` of a table-data tab. */
export interface TableDataPanelParams {
  editorId: string;
  sessionId: string;
  table: ObjectRef;
}

/** `params` of a table designer tab; `isNew` designs a new table in `table`'s schema. */
export interface DesignerPanelParams {
  sessionId: string;
  table: ObjectRef;
  isNew?: boolean;
}

/** Whether two refs name the same object (kind, catalog, schema, name). */
export function sameObject(a: ObjectRef, b: ObjectRef): boolean {
  return (
    a.kind === b.kind &&
    a.name === b.name &&
    (a.catalog ?? "") === (b.catalog ?? "") &&
    (a.schema ?? "") === (b.schema ?? "")
  );
}

function neighbourPanel(api: DockviewApi): string | undefined {
  return api.panels.find(
    (p) =>
      p.id === "welcome" ||
      p.id.startsWith("editor-") ||
      p.id.startsWith("data-") ||
      p.id.startsWith("designer-") ||
      p.id.startsWith("collection-") ||
      p.id.startsWith("keys-") ||
      p.id.startsWith("console-") ||
      p.id.startsWith("pubsub-"),
  )?.id;
}

/**
 * Opens a table's rows in a table-data tab (or shows the tab already open for it). Returns the
 * tab's editor id.
 */
export function openTableData(init: { sessionId: string; table: ObjectRef }): string | null {
  const api = dockview;
  if (!api) return null;
  const existing = api.panels.find((p) => {
    const params = p.params as Partial<TableDataPanelParams> | undefined;
    return (
      p.id.startsWith("data-") &&
      params?.sessionId === init.sessionId &&
      params.table !== undefined &&
      sameObject(params.table, init.table)
    );
  });
  if (existing) {
    existing.api.setActive();
    return (existing.params as TableDataPanelParams).editorId;
  }
  const editorId = newEditorId();
  const params: TableDataPanelParams = { editorId, ...init };
  const neighbour = neighbourPanel(api);
  api.addPanel({
    id: `data-${editorId}`,
    component: "tableData",
    title: qualifiedName(init.table),
    params,
    ...(neighbour ? { position: { referencePanel: neighbour, direction: "within" } } : {}),
  });
  return editorId;
}

/** The title of a table designer tab. */
export function designerTitle(params: DesignerPanelParams): string {
  return params.isNew ? "New table" : `Design ${qualifiedName(params.table)}`;
}

/** Opens the table designer for a table, or for a new table when `isNew`. */
export function openTableDesigner(init: DesignerPanelParams): void {
  const api = dockview;
  if (!api) return;
  if (!init.isNew) {
    const existing = api.panels.find((p) => {
      const params = p.params as Partial<DesignerPanelParams> | undefined;
      return (
        p.id.startsWith("designer-") &&
        !params?.isNew &&
        params?.sessionId === init.sessionId &&
        params.table !== undefined &&
        sameObject(params.table, init.table)
      );
    });
    if (existing) {
      existing.api.setActive();
      return;
    }
  }
  counter += 1;
  const neighbour = neighbourPanel(api);
  api.addPanel({
    id: `designer-${Date.now().toString(36)}-${counter}`,
    component: "designer",
    title: designerTitle(init),
    params: init,
    ...(neighbour ? { position: { referencePanel: neighbour, direction: "within" } } : {}),
  });
}

// --- M7: MongoDB collections, Redis key browser, console and pub/sub ---------------------

/** `params` of a collection tab (MongoDB documents). */
export interface CollectionPanelParams {
  editorId: string;
  sessionId: string;
  /** `kind: "table"` (a collection) or `"view"`; `catalog` = the database. */
  collection: ObjectRef;
}

/** Opens a collection's documents (or shows the tab already open for it). */
export function openCollection(init: { sessionId: string; collection: ObjectRef }): string | null {
  const api = dockview;
  if (!api) return null;
  const existing = api.panels.find((p) => {
    const params = p.params as Partial<CollectionPanelParams> | undefined;
    return (
      p.id.startsWith("collection-") &&
      params?.sessionId === init.sessionId &&
      params.collection !== undefined &&
      sameObject(params.collection, init.collection)
    );
  });
  if (existing) {
    existing.api.setActive();
    return (existing.params as CollectionPanelParams).editorId;
  }
  const editorId = newEditorId();
  const params: CollectionPanelParams = { editorId, ...init };
  const neighbour = neighbourPanel(api);
  api.addPanel({
    id: `collection-${editorId}`,
    component: "collection",
    title: qualifiedName(init.collection),
    params,
    ...(neighbour ? { position: { referencePanel: neighbour, direction: "within" } } : {}),
  });
  return editorId;
}

/** `params` of a key browser tab (Redis). */
export interface KeyBrowserParams {
  editorId: string;
  sessionId: string;
  /** Logical database index as text (`"0"`); the session's own when absent. */
  database?: string;
  /** `MATCH` pattern to scan with (a namespace's `prefix*`). */
  pattern?: string;
  /** A key to open in the value editor (not `key`: React reserves that prop). */
  openKey?: BinaryText;
  /** Bumped by every open request, so the tab applies a repeated one again. */
  request?: number;
}

let keyRequests = 0;

/** Opens the key browser of a session's database, or points the open one at `pattern` / `key`. */
export function openKeyBrowser(init: {
  sessionId: string;
  database?: string;
  pattern?: string;
  openKey?: BinaryText;
}): void {
  const api = dockview;
  if (!api) return;
  keyRequests += 1;
  const existing = api.panels.find((p) => {
    const params = p.params as Partial<KeyBrowserParams> | undefined;
    return (
      p.id.startsWith("keys-") &&
      params?.sessionId === init.sessionId &&
      (params.database ?? "") === (init.database ?? "")
    );
  });
  if (existing) {
    // dockview merges params: unset fields are cleared explicitly.
    existing.api.updateParameters({
      pattern: init.pattern,
      openKey: init.openKey,
      request: keyRequests,
    });
    existing.api.setActive();
    return;
  }
  const editorId = newEditorId();
  const params: KeyBrowserParams = { editorId, ...init, request: keyRequests };
  const neighbour = neighbourPanel(api);
  api.addPanel({
    id: `keys-${editorId}`,
    component: "keys",
    title: `Keys${init.database ? ` db${init.database}` : ""}`,
    params,
    ...(neighbour ? { position: { referencePanel: neighbour, direction: "within" } } : {}),
  });
}

/** `params` of a Redis console tab. */
export interface ConsolePanelParams {
  editorId: string;
  sessionId: string;
  database?: string;
}

/** Opens a new console on a session (each tab has its own editor session). */
export function openConsole(init: { sessionId: string; database?: string }): string | null {
  const api = dockview;
  if (!api) return null;
  const editorId = newEditorId();
  const params: ConsolePanelParams = { editorId, ...init };
  const neighbour = neighbourPanel(api);
  api.addPanel({
    id: `console-${editorId}`,
    component: "console",
    title: `Console ${counter}`,
    params,
    ...(neighbour ? { position: { referencePanel: neighbour, direction: "within" } } : {}),
  });
  return editorId;
}

/** `params` of a publish / subscribe tab. */
export interface PubSubPanelParams {
  sessionId: string;
}

/** Opens the publish / subscribe tab of a session (one per session). */
export function openPubSub(init: PubSubPanelParams): void {
  const api = dockview;
  if (!api) return;
  const existing = api.panels.find(
    (p) =>
      p.id.startsWith("pubsub-") &&
      (p.params as Partial<PubSubPanelParams> | undefined)?.sessionId === init.sessionId,
  );
  if (existing) {
    existing.api.setActive();
    return;
  }
  counter += 1;
  const neighbour = neighbourPanel(api);
  api.addPanel({
    id: `pubsub-${Date.now().toString(36)}-${counter}`,
    component: "pubsub",
    title: "Pub/Sub",
    params: init,
    ...(neighbour ? { position: { referencePanel: neighbour, direction: "within" } } : {}),
  });
}
