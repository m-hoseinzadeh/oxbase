/**
 * Typed wrappers for the M3 table data and grid editing commands of `crates/app-tauri`
 * (src/data.rs). Table data is an ordinary query run: page it with `queryPage`, stop it with
 * `queryCancel`, close it with `queryClose`. The pending-changes buffer lives in the UI
 * (helpers in `./pending`) and goes whole to `editApply`.
 */
import { Channel } from "@tauri-apps/api/core";
import { call } from "./call";
import type {
  ApplyReport,
  Editability,
  EditorId,
  FormRow,
  PendingChanges,
  RunEvent,
  RunId,
  RunOptions,
  TableDataQuery,
} from "./types";

/**
 * Opens a table's rows (filtered and sorted) in editor `editorId`. `onEvent` receives the run's
 * events like `queryRun`; `requestId` cancels the table-definition load with `cancelRequest`.
 */
export function tableDataOpen(
  sessionId: string,
  editorId: EditorId,
  request: TableDataQuery,
  onEvent: (event: RunEvent) => void,
  options?: RunOptions,
  requestId?: string,
): Promise<RunId> {
  const channel = new Channel<RunEvent>();
  channel.onmessage = onEvent;
  return call("table_data_open", {
    sessionId,
    editorId,
    request,
    options: options ?? null,
    onEvent: channel,
    requestId: requestId ?? null,
  });
}

/** Whether and how the run's result can be edited (read-only results say why). */
export function editBegin(runId: RunId, requestId?: string): Promise<Editability> {
  return call("edit_begin", { runId, requestId: requestId ?? null });
}

/** Applies the pending changes in one transaction; per-edit outcomes in the report. */
export function editApply(
  runId: RunId,
  changes: PendingChanges,
  requestId?: string,
): Promise<ApplyReport> {
  return call("edit_apply", { runId, changes, requestId: requestId ?? null });
}

/** One row for the form / JSON view. */
export function rowGetForm(runId: RunId, resultIndex: number, row: number): Promise<FormRow> {
  return call("row_get_form", { runId, resultIndex, row });
}
