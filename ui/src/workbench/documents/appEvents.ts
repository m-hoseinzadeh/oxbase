/**
 * 1.3: events from Rust (crates/app-tauri/src/shell.rs): files the OS or the command line asked
 * the app to open, and the app about to quit. Subscribed once, after the workspace came back,
 * so files opened by a double-click end up in front of the restored tabs.
 *
 * - A query file opens in a query tab (nothing runs).
 * - A SQLite database gets its saved connection (Rust creates one the first time), which the
 *   Connections panel then connects.
 * - A file that could not be opened says why.
 * - Quitting: unless turned off in Settings, the user is asked first ("Quit Oxbase?", with the
 *   tables whose pending changes would be lost); Rust holds the close meanwhile
 *   (`app_close_wait`), and Cancel keeps the app open (`app_close_cancel`). Then the workspace
 *   is saved, and Rust is told to close.
 */
import {
  type AppEvent,
  appCloseCancel,
  appCloseReady,
  appCloseWait,
  appEventsSubscribe,
  type OpenRequest,
} from "../../api";
import { requestConnect } from "../../connections/requests";
import { getConnections, refreshProfiles } from "../../connections/store";
import { getSettings, updateSettings } from "../../settings";
import { tablesWithUnappliedChanges } from "../unappliedChanges";
import { showOpenedDocument } from "./actions";
import { askQuit, showFileError } from "./dialogs";
import { flushWorkspace } from "./workspace";

async function open(requests: OpenRequest[]): Promise<void> {
  const failures: string[] = [];
  for (const request of requests) {
    switch (request.kind) {
      case "document":
        showOpenedDocument(request.opened);
        break;
      case "sqlite":
        if (request.created) await refreshProfiles();
        requestConnect(request.profileId);
        break;
      case "failed":
        failures.push(`${request.name}: ${request.message}`);
        break;
    }
  }
  if (failures.length > 0) {
    await showFileError(
      failures.length === 1 ? "Could not open the file" : "Could not open some files",
      failures.join("\n"),
    );
  }
}

/** Keeps the workspace, then lets the window go (Rust closes anyway after a timeout). */
function quit(): void {
  void flushWorkspace()
    .catch(() => {})
    .finally(() => {
      void appCloseReady().catch(() => {});
    });
}

/** The question is on screen: a second close request while it is open changes nothing. */
let asking = false;

async function askThenQuit(): Promise<void> {
  asking = true;
  // First, so Rust's timeout does not close the app under the question.
  void appCloseWait().catch(() => {});
  try {
    const answer = await askQuit({
      connections: getConnections().sessions.length,
      unapplied: tablesWithUnappliedChanges(),
    });
    if (!answer.quit) {
      void appCloseCancel().catch(() => {});
      return;
    }
    if (answer.dontAskAgain) updateSettings({ confirmQuit: false });
    quit();
  } finally {
    asking = false;
  }
}

/** Handles one event from Rust. */
export function handleAppEvent(event: AppEvent): void {
  if (event.type === "open") {
    void open(event.requests);
    return;
  }
  if (asking) return;
  if (getSettings().confirmQuit) void askThenQuit();
  else quit();
}

/** Subscribes to the events (no-op without the desktop core). */
export function installAppEvents(): void {
  void appEventsSubscribe(handleAppEvent).catch(() => {});
}
