/**
 * 1.3: the workspace, the open query tabs kept between app runs (docs/files.md). At start the
 * tabs of the last run come back (files are read again; unsaved text comes back as it was). While
 * the app runs, every change to the query tabs saves the workspace a second later, and quitting
 * saves it once more before the app closes (`flushWorkspace`, called on `closeRequested`).
 *
 * Only query tabs are kept, with their text when it is not the file's (unsaved changes, or no
 * file), their connection (the saved profile, not the session) and database.
 */
import {
  type RestoredWorkspace,
  type WorkspaceInput,
  type WorkspaceTabInput,
  workspaceLoad,
  workspaceSave,
} from "../../api";
import {
  activatePanel,
  activePanelId,
  allPanelIds,
  dockviewApi,
  openQueryEditor,
  panelTitle,
} from "../layout";
import { showNotice } from "./dialogs";
import { isDirty, isQueryTab, subscribeTabs, tabController, tabDocument } from "./tabs";

/** How long after the last change the workspace is saved. */
export const SAVE_DELAY_MS = 1000;

let started = false;
let timer: ReturnType<typeof setTimeout> | null = null;
let pending: Promise<void> | null = null;
/** The last workspace sent, so an unchanged one is not sent again. */
let lastSent = "";

/** The workspace as it is now. */
export function snapshotWorkspace(): WorkspaceInput {
  const ids = allPanelIds().filter(isQueryTab);
  const active = activePanelId();
  const tabs = ids.map((id): WorkspaceTabInput => {
    const controller = tabController(id);
    const document = tabDocument(id);
    const target = controller?.target() ?? {};
    const keepText = !document || isDirty(id);
    return {
      title: panelTitle(id),
      ...(document ? { document: document.id } : {}),
      ...(keepText && controller ? { text: controller.getText() } : {}),
      ...(target.profileId ? { profileId: target.profileId } : {}),
      ...(target.database ? { database: target.database } : {}),
    };
  });
  const index = active ? ids.indexOf(active) : -1;
  return { tabs, ...(index >= 0 ? { active: index } : {}) };
}

async function send(): Promise<void> {
  const workspace = snapshotWorkspace();
  const text = JSON.stringify(workspace);
  if (text === lastSent) return;
  try {
    await workspaceSave(workspace);
    lastSent = text;
  } catch {
    // Kept on the next change; a failed save never interrupts the user.
  }
}

function schedule() {
  if (!started) return;
  if (timer) clearTimeout(timer);
  timer = setTimeout(() => {
    timer = null;
    pending = send().finally(() => {
      pending = null;
    });
  }, SAVE_DELAY_MS);
}

/** Saves the workspace now (quitting). */
export async function flushWorkspace(): Promise<void> {
  if (!started) return;
  if (timer) {
    clearTimeout(timer);
    timer = null;
  }
  if (pending) await pending;
  await send();
}

/** Opens the tabs of `restored` behind the active tab, then shows the one that was active. */
export function openRestoredTabs(restored: RestoredWorkspace): void {
  const ids = restored.tabs.map((tab) =>
    openQueryEditor(
      {
        sql: tab.text,
        title: tab.title,
        ...(tab.document ? { document: tab.document } : {}),
        ...(tab.profileId ? { profileId: tab.profileId } : { pickConnection: true }),
        ...(tab.database ? { database: tab.database } : {}),
        ...(tab.dirty ? { restoredDirty: true } : {}),
      },
      { inactive: true },
    ),
  );
  const active = restored.active === undefined ? null : ids[restored.active];
  const first = ids.find((id) => id !== null);
  if (active) activatePanel(active);
  else if (first) activatePanel(first);
}

/**
 * Brings back the last run's tabs, then starts keeping the workspace. Called once when the
 * layout is ready.
 */
export async function restoreWorkspace(): Promise<void> {
  let restored: RestoredWorkspace | null = null;
  try {
    restored = await workspaceLoad();
  } catch {
    // No core (a plain browser, tests) or an unreadable workspace: start empty.
  }
  if (restored && restored.tabs.length > 0) openRestoredTabs(restored);
  startWorkspace();
  if (restored && restored.skipped.length > 0) {
    const names = restored.skipped.join(", ");
    void showNotice(
      "Some files were not reopened",
      `${names} could not be opened again. ${
        restored.skipped.length === 1 ? "It was" : "They were"
      } moved, renamed or deleted since the last session.`,
    );
  }
}

/** Starts saving the workspace on changes. */
export function startWorkspace(): void {
  if (started) return;
  started = true;
  subscribeTabs(schedule);
  const api = dockviewApi();
  api?.onDidLayoutChange(schedule);
  api?.onDidActivePanelChange(schedule);
}

/** Tests only. */
export function resetWorkspace(): void {
  started = false;
  if (timer) clearTimeout(timer);
  timer = null;
  pending = null;
  lastSent = "";
}
