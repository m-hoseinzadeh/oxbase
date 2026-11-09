/**
 * 1.3: what each open query tab shows (a file, a saved query, or nothing yet) and whether it
 * has unsaved changes. A query tab registers a controller while it is mounted; save, close,
 * the Queries panel and the workspace go through here.
 *
 * Unsaved changes follow the editor's version (Monaco's alternative version id): the tab is
 * dirty while the version differs from the one it had when it was last saved or opened, so
 * undoing back to the saved text makes it clean again. A tab restored with unsaved text stays
 * dirty until it is saved.
 */
import { useSyncExternalStore } from "react";
import type { DocumentInfo, FileLink } from "../../api";

/** What the document code needs from a query tab. */
export interface QueryTabController {
  getText(): string;
  /** The editor's current version (see the module docs). */
  getVersion(): number;
  /** The connection the tab runs on, if any: profile, database and its display name. */
  target(): { profileId?: string; database?: string; profileName?: string };
}

interface TabEntry {
  controller: QueryTabController;
  document: DocumentInfo | null;
  /** The version that matches the file; `null` = unknown (restored unsaved text). */
  savedVersion: number | null;
  /** The editor reported a version yet (the editor loads lazily). */
  known: boolean;
  dirty: boolean;
}

const tabs = new Map<string, TabEntry>();
const listeners = new Set<() => void>();
let generation = 0;

function changed() {
  generation += 1;
  for (const listener of listeners) listener();
}

/** Calls `listener` whenever a tab's document, dirty state or text version changes. */
export function subscribeTabs(listener: () => void): () => void {
  listeners.add(listener);
  return () => listeners.delete(listener);
}

/**
 * Registers query tab `panelId`. `dirty`: it starts with unsaved text (restored). Returns the
 * unregister.
 */
export function registerQueryTab(
  panelId: string,
  controller: QueryTabController,
  init: { document?: DocumentInfo | undefined; dirty?: boolean | undefined } = {},
): () => void {
  const entry: TabEntry = {
    controller,
    document: init.document ?? null,
    savedVersion: init.dirty ? null : 1,
    known: false,
    dirty: init.dirty ?? false,
  };
  tabs.set(panelId, entry);
  changed();
  return () => {
    if (tabs.get(panelId) === entry) {
      tabs.delete(panelId);
      changed();
    }
  };
}

/** The editor of `panelId` is ready at `version` (the text it opened with). */
export function noteEditorReady(panelId: string, version: number): void {
  const entry = tabs.get(panelId);
  if (!entry || entry.known) return;
  entry.known = true;
  if (entry.savedVersion !== null) entry.savedVersion = version;
  entry.dirty = entry.savedVersion === null;
  changed();
}

/** The text of `panelId` changed; it is now at `version`. */
export function noteVersion(panelId: string, version: number): void {
  const entry = tabs.get(panelId);
  if (!entry) return;
  entry.known = true;
  entry.dirty = entry.savedVersion === null || version !== entry.savedVersion;
  changed();
}

/** `panelId` was saved as `document`: its current text is the file's. */
export function markSaved(panelId: string, document: DocumentInfo): void {
  const entry = tabs.get(panelId);
  if (!entry) return;
  entry.document = document;
  entry.savedVersion = entry.controller.getVersion();
  entry.dirty = false;
  changed();
}

/** Every open tab of document `id` now shows `document` (a renamed saved query). */
export function updateDocument(document: DocumentInfo): string[] {
  const ids: string[] = [];
  for (const [panelId, entry] of tabs) {
    if (entry.document?.id === document.id) {
      entry.document = document;
      ids.push(panelId);
    }
  }
  if (ids.length > 0) changed();
  return ids;
}

/** Document `id` is gone (a deleted saved query): its tabs keep their text, unsaved. */
export function detachDocument(id: string): string[] {
  const ids: string[] = [];
  for (const [panelId, entry] of tabs) {
    if (entry.document?.id === id) {
      entry.document = null;
      entry.savedVersion = null;
      entry.dirty = true;
      ids.push(panelId);
    }
  }
  if (ids.length > 0) changed();
  return ids;
}

export function isQueryTab(panelId: string): boolean {
  return tabs.has(panelId);
}

export function isDirty(panelId: string): boolean {
  return tabs.get(panelId)?.dirty ?? false;
}

export function tabDocument(panelId: string): DocumentInfo | null {
  return tabs.get(panelId)?.document ?? null;
}

export function tabController(panelId: string): QueryTabController | null {
  return tabs.get(panelId)?.controller ?? null;
}

/** The query tab showing document `id`, if one is open. */
export function tabOfDocument(id: string): string | null {
  for (const [panelId, entry] of tabs) if (entry.document?.id === id) return panelId;
  return null;
}

/** The connection link of `panelId`, if it runs on a saved connection. */
export function tabLink(panelId: string): FileLink | null {
  const target = tabs.get(panelId)?.controller.target();
  if (!target?.profileId) return null;
  return { profileId: target.profileId, ...(target.database ? { database: target.database } : {}) };
}

/** Every registered query tab id. */
export function queryTabIds(): string[] {
  return [...tabs.keys()];
}

/** Whether tab `panelId` has unsaved changes, re-rendering when that changes. */
export function useTabDirty(panelId: string): boolean {
  return useSyncExternalStore(subscribeTabs, () => isDirty(panelId));
}

/** Changes on every tab change (for effects that follow all tabs). */
export function tabsGeneration(): number {
  return generation;
}

/** Tests only. */
export function resetTabs(): void {
  tabs.clear();
  changed();
}
