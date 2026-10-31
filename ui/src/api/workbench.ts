/**
 * Typed wrappers for the M2 workbench commands of `crates/app-tauri` (src/workbench.rs).
 * Every failure is normalised to a `{ category, message }` CommandError, like the connection
 * commands. Result pages come back as Arrow IPC bytes and are decoded here, never as JSON.
 */
import { Channel, invoke, isTauri } from "@tauri-apps/api/core";
import type { Table } from "apache-arrow";
import { toCommandError } from "../connections/api";
import { decodeArrowPage, NoCoreError, type RawBytes } from "../ipc";
import type {
  EditorId,
  Engine,
  HistoryEntry,
  HistoryQuery,
  HistorySettings,
  ObjectDef,
  ObjectRef,
  RunEvent,
  RunId,
  RunOptions,
  StatementSpan,
  TreeNode,
  TreeNodeRef,
  TreeOptions,
} from "./types";

async function call<T>(cmd: string, args?: Record<string, unknown>): Promise<T> {
  if (!isTauri()) throw toCommandError(new NoCoreError());
  try {
    return await invoke<T>(cmd, args);
  } catch (error) {
    throw toCommandError(error);
  }
}

/** Children of a tree node. `requestId` makes the call cancellable with `cancelRequest`. */
export function treeChildren(
  sessionId: string,
  node: TreeNodeRef,
  options?: TreeOptions,
  requestId?: string,
): Promise<TreeNode[]> {
  return call("tree_children", {
    sessionId,
    node,
    options: options ?? null,
    requestId: requestId ?? null,
  });
}

/** The full definition of one object. */
export function objectLoad(
  sessionId: string,
  object: ObjectRef,
  requestId?: string,
): Promise<ObjectDef> {
  return call("object_load", { sessionId, object, requestId: requestId ?? null });
}

/**
 * Starts a run. `onEvent` receives every RunEvent in order; the promise resolves with the run
 * id as soon as the run started (before its events are done).
 */
export function queryRun(
  sessionId: string,
  editorId: EditorId,
  script: string,
  onEvent: (event: RunEvent) => void,
  options?: RunOptions,
): Promise<RunId> {
  const channel = new Channel<RunEvent>();
  channel.onmessage = onEvent;
  return call("query_run", {
    sessionId,
    editorId,
    script,
    options: options ?? null,
    onEvent: channel,
  });
}

/** Rows `[offset, offset + limit)` of result `resultIndex`, decoded from Arrow IPC. */
export async function queryPage(
  runId: RunId,
  resultIndex: number,
  offset: number,
  limit: number,
): Promise<Table> {
  const raw = await call<RawBytes>("query_page", { runId, resultIndex, offset, limit });
  return decodeArrowPage(raw);
}

/** Cancels a running run; resolves to whether it was running. */
export function queryCancel(runId: RunId): Promise<boolean> {
  return call("query_cancel", { runId });
}

/** Closes a run and deletes its stored results. */
export function queryClose(runId: RunId): Promise<void> {
  return call("query_close", { runId });
}

/** Closes an editor tab: its runs and its database session. */
export function queryCloseEditor(editorId: EditorId): Promise<void> {
  return call("query_close_editor", { editorId });
}

/** The statement under the cursor; `offset` is Monaco's `model.getOffsetAt(position)`. */
export function statementAt(
  engine: Engine,
  text: string,
  offset: number,
): Promise<StatementSpan | null> {
  return call("statement_at", { engine, text, offset });
}

export function historyList(query?: HistoryQuery): Promise<HistoryEntry[]> {
  return call("history_list", { query: query ?? null });
}

export function historyDelete(ids: string[]): Promise<number> {
  return call("history_delete", { ids });
}

/** Clears the history of one profile, or all of it when `profileId` is omitted. */
export function historyClear(profileId?: string): Promise<number> {
  return call("history_clear", { profileId: profileId ?? null });
}

export function historyGetSettings(): Promise<HistorySettings> {
  return call("history_get_settings");
}

export function historySetSettings(settings: HistorySettings): Promise<void> {
  return call("history_set_settings", { settings });
}
