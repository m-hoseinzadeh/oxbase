/**
 * Opens the M8 dialogs from anywhere (app bar, object tree): the data transfer wizard and the
 * backup / restore dialogs. One is shown at a time by `SyncHost`; while it runs, requests to
 * open another are refused (the run would lose its view). The structure and data sync views are
 * workbench tabs (`openStructureSync` / `openDataSync` in `../layout`).
 */
import { useSyncExternalStore } from "react";
import type { ObjectRef } from "../../api";

/** A connection and, when known, a database or schema of it (or a table in it). */
export interface SideInit {
  sessionId: string;
  /** A database or schema ref. */
  container?: ObjectRef;
}

export interface TransferInit {
  source?: SideInit;
  /** Tables or views to start with (in `source.container`). */
  objects?: ObjectRef[];
}

export interface BackupInit {
  source?: SideInit;
}

export interface RestoreInit {
  target?: SideInit;
  /** Only check an archive (no restore). */
  verifyOnly?: boolean;
}

export type SyncDialog =
  | { kind: "transfer"; key: number; init: TransferInit }
  | { kind: "backup"; key: number; init: BackupInit }
  | { kind: "restore"; key: number; init: RestoreInit }
  | null;

let current: SyncDialog = null;
let busy = false;
let counter = 0;
const listeners = new Set<() => void>();

function set(next: SyncDialog) {
  current = next;
  busy = false;
  for (const listener of listeners) listener();
}

function subscribe(listener: () => void): () => void {
  listeners.add(listener);
  return () => listeners.delete(listener);
}

function open(next: (key: number) => SyncDialog): boolean {
  if (busy) return false;
  counter += 1;
  set(next(counter));
  return true;
}

/** Opens the data transfer wizard. `false` while a run is going on in the open dialog. */
export function openTransfer(init: TransferInit = {}): boolean {
  return open((key) => ({ kind: "transfer", key, init }));
}

export function openBackup(init: BackupInit = {}): boolean {
  return open((key) => ({ kind: "backup", key, init }));
}

export function openRestore(init: RestoreInit = {}): boolean {
  return open((key) => ({ kind: "restore", key, init }));
}

export function closeSyncDialog(): void {
  set(null);
}

/** The open dialog says whether a run is in progress (it then cannot be replaced). */
export function setSyncBusy(value: boolean): void {
  busy = value;
}

export function useSyncDialog(): SyncDialog {
  return useSyncExternalStore(subscribe, () => current);
}

/** Tests only: no dialog. */
export function resetSyncStore(): void {
  set(null);
}
