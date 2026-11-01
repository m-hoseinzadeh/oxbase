/**
 * Opens the import wizard and the export dialog from anywhere (main toolbar, object tree, grid,
 * query editor). One transfer dialog is shown at a time by `TransferHost`; while it runs an
 * import or export, other requests to open one are refused (the run would lose its view).
 */
import { useSyncExternalStore } from "react";
import type { Engine, ExportSource, ObjectRef } from "../../api";

/** Where the import wizard starts: a connection and an object of the tree, if any. */
export interface ImportInit {
  sessionId?: string;
  /** A database or schema (create a table in it) or a table (import into it). */
  target?: ObjectRef;
}

/** What an export writes and how the dialog names it. */
export interface ExportInit {
  source: ExportSource;
  /** Engine of the source (the default SQL dump target). */
  engine?: Engine;
  /** What is exported, for the dialog ("Result 1", "Table public.users", ...). */
  label: string;
  /** Suggested file name without extension. */
  baseName: string;
  /** A table export offers a filter and a sort (export from the tree). */
  offerFilter?: boolean;
  /** Rows the source holds, when known (progress and ETA). */
  totalRows?: number;
}

export type TransferDialog =
  | { kind: "import"; key: number; init: ImportInit }
  | { kind: "export"; key: number; init: ExportInit }
  | null;

let current: TransferDialog = null;
let busy = false;
let counter = 0;
const listeners = new Set<() => void>();

function set(next: TransferDialog) {
  current = next;
  busy = false;
  for (const listener of listeners) listener();
}

function subscribe(listener: () => void): () => void {
  listeners.add(listener);
  return () => listeners.delete(listener);
}

/** Opens the import wizard. `false` when a transfer is running in the open dialog. */
export function openImport(init: ImportInit = {}): boolean {
  if (busy) return false;
  counter += 1;
  set({ kind: "import", key: counter, init });
  return true;
}

/** Opens the export dialog. `false` when a transfer is running in the open dialog. */
export function openExport(init: ExportInit): boolean {
  if (busy) return false;
  counter += 1;
  set({ kind: "export", key: counter, init });
  return true;
}

export function closeTransferDialog(): void {
  set(null);
}

/** The open dialog says whether a run is in progress (it then cannot be replaced). */
export function setTransferBusy(value: boolean): void {
  busy = value;
}

export function useTransferDialog(): TransferDialog {
  return useSyncExternalStore(subscribe, () => current);
}

/** Tests only: no dialog. */
export function resetTransferStore(): void {
  set(null);
}
