/**
 * 1.3: opening, saving and closing query files and saved queries (docs/files.md).
 *
 * - Open: a file from the native dialog (Ctrl+O), a recent file, a saved query, or a file the OS
 *   opened. A document already open in a tab only brings that tab forward. A file that never
 *   ran anywhere opens without a connection chosen; one with a link opens on its connection.
 * - Save (Ctrl+S): to the tab's file or saved query. A new tab on a saved connection asks for a
 *   saved query name (or "Save to a file instead…"); without a connection it asks for a file.
 * - Save as (Ctrl+Shift+S): always a file, from the native save dialog.
 * - Close: a tab with unsaved changes asks first (Save / Don't save / Cancel).
 */
import {
  type DocumentInfo,
  documentOpen,
  documentRecent,
  documentRecentClear,
  documentReopen,
  documentSave,
  documentSaveAs,
  documentSetLink,
  type FileLink,
  type OpenedDocument,
  savedQueryCreate,
} from "../../api";
import { toCommandError } from "../../connections/api";
import {
  activatePanel,
  closePanel,
  groupPanelIds,
  openQueryEditor,
  panelTitle,
  setPanelTitle,
} from "../layout";
import { askName, askUnsaved, showFileError } from "./dialogs";
import {
  isDirty,
  isQueryTab,
  markSaved,
  queryTabIds,
  tabController,
  tabDocument,
  tabLink,
  tabOfDocument,
} from "./tabs";

const savedQueryListeners = new Set<() => void>();

/** Calls `listener` when saved queries were created, renamed or deleted. */
export function onSavedQueriesChanged(listener: () => void): () => void {
  savedQueryListeners.add(listener);
  return () => savedQueryListeners.delete(listener);
}

export function notifySavedQueriesChanged(): void {
  for (const listener of savedQueryListeners) listener();
}

/** The title a tab shows for a document. */
export function documentTitle(document: DocumentInfo): string {
  return document.name;
}

/** Shows `opened` in a query tab: its open tab, or a new one. Returns the tab id. */
export function showOpenedDocument(opened: OpenedDocument): string | null {
  const existing = tabOfDocument(opened.document.id);
  if (existing) {
    activatePanel(existing);
    return existing;
  }
  const link = opened.link;
  return openQueryEditor({
    sql: opened.text,
    title: documentTitle(opened.document),
    document: opened.document,
    ...(link ? { profileId: link.profileId } : { pickConnection: true }),
    ...(link?.database ? { database: link.database } : {}),
  });
}

async function opening(work: () => Promise<OpenedDocument | null>, what: string): Promise<void> {
  try {
    const opened = await work();
    if (opened) showOpenedDocument(opened);
  } catch (error) {
    await showFileError(`Could not open ${what}`, toCommandError(error).message);
  }
}

/** Ctrl+O: picks a file and opens it in a query tab. */
export function openFileDialog(): Promise<void> {
  return opening(documentOpen, "the file");
}

/** Opens a recent file or a saved query by id. */
export function openDocumentById(id: string, name: string): Promise<void> {
  const existing = tabOfDocument(id);
  if (existing) {
    activatePanel(existing);
    return Promise.resolve();
  }
  return opening(() => documentReopen(id), name);
}

export { documentRecent as recentFiles, documentRecentClear as clearRecentFiles };

/** A file name for `title` (characters files cannot have become `_`), ending in `.sql`. */
export function suggestedFileName(title: string): string {
  const base =
    title
      .replace(/[\\/:*?"<>|\p{Cc}]/gu, "_")
      .replace(/^\.+/, "")
      .trim() || "query";
  return /\.[A-Za-z0-9]{1,8}$/.test(base) ? base : `${base}.sql`;
}

function saved(panelId: string, document: DocumentInfo) {
  markSaved(panelId, document);
  setPanelTitle(panelId, documentTitle(document));
}

async function failed(what: string, error: unknown): Promise<false> {
  await showFileError(`Could not save ${what}`, toCommandError(error).message);
  return false;
}

/** Save as: a file from the native dialog. `true` when saved. */
export async function saveTabAs(panelId: string): Promise<boolean> {
  const controller = tabController(panelId);
  if (!controller) return false;
  const document = tabDocument(panelId);
  const name = document
    ? document.savedQuery
      ? `${document.name}.sql`
      : document.name
    : suggestedFileName(panelTitle(panelId));
  const link = tabLink(panelId);
  try {
    const info = await documentSaveAs(name, controller.getText(), {
      ...(document ? { from: document.id } : {}),
      ...(link ? { link } : {}),
    });
    if (!info) return false;
    saved(panelId, info);
    return true;
  } catch (error) {
    return failed(name, error);
  }
}

/** Saves the tab as a new saved query of its connection. `true` when saved. */
export async function saveTabAsQuery(panelId: string, offerFile = false): Promise<boolean> {
  const controller = tabController(panelId);
  const target = controller?.target();
  if (!controller || !target?.profileId) {
    await showFileError(
      "Choose a connection first",
      "A saved query belongs to a saved connection. Choose the connection in the tab's toolbar, or save to a file.",
    );
    return false;
  }
  const profileId = target.profileId;
  const document = tabDocument(panelId);
  const title = panelTitle(panelId);
  const initial = document?.savedQuery ? `${document.name} copy` : title.replace(/\.sql$/i, "");
  const answer = await askName({
    title: "Save query",
    label: "Query name",
    initial,
    action: "Save",
    note: `Saved with the connection ${target.profileName ?? ""}${
      target.database ? `, database ${target.database}` : ""
    }. It is listed in the Queries panel.`,
    offerFile,
    submit: async (name) => {
      try {
        const info = await savedQueryCreate(profileId, name, controller.getText(), target.database);
        saved(panelId, info);
        notifySavedQueriesChanged();
        return null;
      } catch (error) {
        return toCommandError(error).message;
      }
    },
  });
  if (answer === "file") return saveTabAs(panelId);
  return answer === "saved";
}

/** Ctrl+S: saves the tab (module docs). `true` when saved. */
export async function saveTab(panelId: string): Promise<boolean> {
  const controller = tabController(panelId);
  if (!controller) return false;
  const document = tabDocument(panelId);
  if (!document) {
    return controller.target().profileId ? saveTabAsQuery(panelId, true) : saveTabAs(panelId);
  }
  try {
    saved(panelId, await documentSave(document.id, controller.getText()));
    return true;
  } catch (error) {
    return failed(document.name, error);
  }
}

/** Saves every query tab with unsaved changes, in order; stops at the first one not saved. */
export async function saveAll(ids: string[] = queryTabIds()): Promise<boolean> {
  for (const id of ids) {
    if (!isDirty(id)) continue;
    activatePanel(id);
    if (!(await saveTab(id))) return false;
  }
  return true;
}

/** Remembers the connection a file tab runs on (after the user picked one). */
export function rememberLink(panelId: string, link: FileLink | null): void {
  const document = tabDocument(panelId);
  if (!document || !link) return;
  void documentSetLink(document.id, link).catch(() => {});
}

/**
 * Closes `ids`, asking once about the ones with unsaved changes. `false` when the user
 * cancelled (nothing is closed then).
 */
export async function closeTabs(ids: string[]): Promise<boolean> {
  const dirty = ids.filter((id) => isQueryTab(id) && isDirty(id));
  if (dirty.length > 0) {
    if (dirty.length === 1) activatePanel(dirty[0] as string);
    const answer = await askUnsaved(dirty.map((id) => panelTitle(id)));
    if (answer === "cancel") return false;
    if (answer === "save" && !(await saveAll(dirty))) return false;
  }
  for (const id of ids) closePanel(id);
  return true;
}

/** A tab's close button, middle click or "Close". */
export function requestClosePanel(panelId: string): Promise<boolean> {
  return closeTabs([panelId]);
}

/** The side panels: "Close others" and "Close all" leave them open. */
const SIDE_PANELS: ReadonlySet<string> = new Set(["connections", "objects", "queries", "history"]);

/** "Close others": the other tabs of the same group (not the side panels). */
export function closeOtherPanels(panelId: string): Promise<boolean> {
  return closeTabs(groupPanelIds(panelId).filter((id) => id !== panelId && !SIDE_PANELS.has(id)));
}

/** "Close all": every tab of the group (not the side panels). */
export function closeGroupPanels(panelId: string): Promise<boolean> {
  return closeTabs(groupPanelIds(panelId).filter((id) => !SIDE_PANELS.has(id)));
}

/** Whether "Close others" / "Close all" apply to the tab (not to the side panels). */
export function isSidePanel(panelId: string): boolean {
  return SIDE_PANELS.has(panelId);
}
