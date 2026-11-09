/**
 * 1.3 query files, saved queries, recent files, the workspace and desktop integration
 * (crates/app-tauri/src/documents.rs and shell.rs, docs/files.md). The UI never sends a path:
 * a tab that shows a file holds its opaque `DocumentId`.
 */
import { Channel } from "@tauri-apps/api/core";
import { call } from "./call";

export type DocumentId = string;

/** The connection and database a file runs on. */
export interface FileLink {
  profileId: string;
  database?: string;
}

export interface DocumentInfo {
  id: DocumentId;
  /** The file name; a saved query's name (without `.sql`). */
  name: string;
  /** For display only: the folder (home as `~`), or "Saved queries". */
  location: string;
  /** Set for a saved query: the connection it belongs to. */
  savedQuery?: { profileId: string };
}

export interface OpenedDocument {
  document: DocumentInfo;
  text: string;
  link?: FileLink;
}

export interface SavedQueryItem {
  document: DocumentInfo;
  profileId: string;
  database?: string;
  modifiedMs: number;
}

export interface WorkspaceTabInput {
  title: string;
  document?: DocumentId;
  /** The tab's text when it is not the file's (unsaved changes, or no file). */
  text?: string;
  profileId?: string;
  database?: string;
}

export interface WorkspaceInput {
  tabs: WorkspaceTabInput[];
  active?: number;
}

export interface RestoredTab {
  title: string;
  document?: DocumentInfo;
  text: string;
  dirty: boolean;
  profileId?: string;
  database?: string;
}

export interface RestoredWorkspace {
  tabs: RestoredTab[];
  active?: number;
  /** Files that could not be opened again; their tabs are gone. */
  skipped: string[];
}

export type OpenRequest =
  | { kind: "document"; opened: OpenedDocument }
  | { kind: "sqlite"; profileId: string; name: string; created: boolean }
  | { kind: "failed"; name: string; message: string };

export type AppEvent = { type: "open"; requests: OpenRequest[] } | { type: "closeRequested" };

/** Native open dialog; the chosen file opened. `null` when cancelled. */
export function documentOpen(): Promise<OpenedDocument | null> {
  return call("document_open");
}

/** A recent file or saved query, by id. */
export function documentReopen(id: DocumentId): Promise<OpenedDocument> {
  return call("document_reopen", { id });
}

/** The recent files that still exist, newest first. */
export function documentRecent(): Promise<DocumentInfo[]> {
  return call("document_recent");
}

export function documentRecentClear(): Promise<void> {
  return call("document_recent_clear");
}

/** Saves the text to the document's file. */
export function documentSave(id: DocumentId, text: string): Promise<DocumentInfo> {
  return call("document_save", { id, text });
}

/** Native save dialog, then saves the text there. `null` when cancelled. */
export function documentSaveAs(
  defaultName: string,
  text: string,
  options: { from?: DocumentId; link?: FileLink } = {},
): Promise<DocumentInfo | null> {
  return call("document_save_as", {
    from: options.from ?? null,
    defaultName,
    text,
    link: options.link ?? null,
  });
}

/** Remembers (or with `null` forgets) the connection a file runs on. */
export function documentSetLink(id: DocumentId, link: FileLink | null): Promise<void> {
  return call("document_set_link", { id, link });
}

export function savedQueryList(): Promise<SavedQueryItem[]> {
  return call("saved_query_list");
}

export function savedQueryCreate(
  profileId: string,
  name: string,
  text: string,
  database?: string,
): Promise<DocumentInfo> {
  return call("saved_query_create", { profileId, name, text, database: database ?? null });
}

export function savedQueryRename(id: DocumentId, name: string): Promise<DocumentInfo> {
  return call("saved_query_rename", { id, name });
}

export function savedQueryDelete(id: DocumentId): Promise<boolean> {
  return call("saved_query_delete", { id });
}

export function workspaceLoad(): Promise<RestoredWorkspace> {
  return call("workspace_load");
}

export function workspaceSave(workspace: WorkspaceInput): Promise<void> {
  return call("workspace_save", { workspace });
}

/** Subscribes to files opened from the OS and to quit requests. */
export function appEventsSubscribe(onEvent: (event: AppEvent) => void): Promise<void> {
  const channel = new Channel<AppEvent>();
  channel.onmessage = onEvent;
  return call("app_events_subscribe", { onEvent: channel });
}

/** The workspace is kept: close the app. */
export function appCloseReady(): Promise<void> {
  return call("app_close_ready");
}
