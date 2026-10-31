/**
 * Shared connection state for the panel and the status bar: saved profiles and open sessions.
 * The Rust core is the source of truth; this only caches what the commands last returned.
 */
import { useSyncExternalStore } from "react";
import { listOpenConnections, listProfiles, toCommandError } from "./api";
import type { CommandError, ConnectionProfile, SessionSummary } from "./types";

export interface ConnectionsSnapshot {
  profilesStatus: "loading" | "ok" | "error";
  profiles: ConnectionProfile[];
  profilesError: CommandError | null;
  sessions: SessionSummary[];
}

const initial: ConnectionsSnapshot = {
  profilesStatus: "loading",
  profiles: [],
  profilesError: null,
  sessions: [],
};

let snapshot: ConnectionsSnapshot = initial;
const listeners = new Set<() => void>();

function update(patch: Partial<ConnectionsSnapshot>) {
  snapshot = { ...snapshot, ...patch };
  for (const listener of listeners) listener();
}

function subscribe(listener: () => void): () => void {
  listeners.add(listener);
  return () => listeners.delete(listener);
}

export async function refreshProfiles(): Promise<void> {
  try {
    update({ profiles: await listProfiles(), profilesStatus: "ok", profilesError: null });
  } catch (error) {
    update({ profilesStatus: "error", profilesError: toCommandError(error) });
  }
}

export async function refreshSessions(): Promise<void> {
  try {
    update({ sessions: await listOpenConnections() });
  } catch {
    update({ sessions: [] });
  }
}

export function refreshAll(): Promise<void> {
  return Promise.all([refreshProfiles(), refreshSessions()]).then(() => undefined);
}

export function useConnections(): ConnectionsSnapshot {
  return useSyncExternalStore(subscribe, () => snapshot);
}

/** Tests only: back to the initial state. */
export function resetConnectionsStore(): void {
  snapshot = initial;
  for (const listener of listeners) listener();
}
