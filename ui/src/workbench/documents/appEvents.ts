/**
 * 1.3: events from Rust (crates/app-tauri/src/shell.rs): files the OS or the command line asked
 * the app to open, and the app about to quit. Subscribed once, after the workspace came back,
 * so files opened by a double-click end up in front of the restored tabs.
 *
 * - A query file opens in a query tab (nothing runs).
 * - A SQLite database gets its saved connection (Rust creates one the first time), which the
 *   Connections panel then connects.
 * - A file that could not be opened says why.
 * - Quitting: the workspace is saved, then Rust is told to close.
 */
import { type AppEvent, appCloseReady, appEventsSubscribe, type OpenRequest } from "../../api";
import { requestConnect } from "../../connections/requests";
import { refreshProfiles } from "../../connections/store";
import { showOpenedDocument } from "./actions";
import { showFileError } from "./dialogs";
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

/** Handles one event from Rust. */
export function handleAppEvent(event: AppEvent): void {
  if (event.type === "open") {
    void open(event.requests);
    return;
  }
  // Closing: keep the workspace, then let the window go (Rust closes anyway after a timeout).
  void flushWorkspace()
    .catch(() => {})
    .finally(() => {
      void appCloseReady().catch(() => {});
    });
}

/** Subscribes to the events (no-op without the desktop core). */
export function installAppEvents(): void {
  void appEventsSubscribe(handleAppEvent).catch(() => {});
}
