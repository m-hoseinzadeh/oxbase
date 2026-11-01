/**
 * The schema cache of a query editor's connection and database (docs/m6-contracts.md §3.5 and
 * §5): its status (no network access), polled every {@link CACHE_POLL_MS} while a build runs,
 * and a refresh that reads the catalog again.
 */
import { useCallback, useEffect, useRef, useState } from "react";
import {
  type CacheState,
  type CacheStatus,
  type RefreshScope,
  schemaCacheRefresh,
  schemaCacheStatus,
} from "../../../api";
import { cancelRequest, newRequestId, toCommandError } from "../../../connections/api";
import type { CommandError } from "../../../connections/types";

/** How often the status is read while a build runs. */
export const CACHE_POLL_MS = 2000;
/** Answers that report another cache state read the status again at most this often. */
export const NOTE_INTERVAL_MS = 2000;
/**
 * How long after a query run the status is read again: a schema change made by the run
 * reaches the cache within 2 s (docs/m6-contracts.md §8), and a new generation makes the
 * editor diagnose again.
 */
export const RUN_SETTLE_MS = 2500;

export interface SchemaCacheView {
  /** `null` before the first answer and without a connection. */
  status: CacheStatus | null;
  /** The last status or refresh failure. */
  error: CommandError | null;
  /** A refresh started here is running. */
  refreshing: boolean;
  /** Reads the catalog again (the whole database). */
  refresh(): void;
  /**
   * An insight answer reported `state`: read the status again if it differs (at once, or once
   * the rate limit allows; a note is never lost).
   */
  noteCacheState(state: CacheState): void;
  /** A query run finished: read the status again once a schema change it made has landed. */
  noteRunFinished(): void;
}

export function useSchemaCache(sessionId: string | null, database: string | null): SchemaCacheView {
  const [status, setStatus] = useState<CacheStatus | null>(null);
  const [error, setError] = useState<CommandError | null>(null);
  const [refreshing, setRefreshing] = useState(false);
  /** Bumped whenever the key changes: answers for an older key are dropped. */
  const generation = useRef(0);
  const refreshId = useRef<string | null>(null);
  const lastLoad = useRef(0);
  const known = useRef<CacheState | null>(null);
  known.current = status?.state ?? null;
  /** Deferred status loads: one for rate-limited notes, one after query runs. */
  const noteTimer = useRef<ReturnType<typeof setTimeout> | null>(null);
  const runTimer = useRef<ReturnType<typeof setTimeout> | null>(null);

  const load = useCallback(() => {
    if (sessionId === null) return;
    const mine = generation.current;
    lastLoad.current = Date.now();
    schemaCacheStatus(sessionId, database).then(
      (next) => {
        if (mine !== generation.current) return;
        setStatus(next);
        setError(null);
      },
      (failure) => {
        if (mine === generation.current) setError(toCommandError(failure));
      },
    );
  }, [sessionId, database]);

  // A new connection or database: forget the old status and cancel its refresh.
  useEffect(() => {
    generation.current += 1;
    setStatus(null);
    setError(null);
    setRefreshing(false);
    load();
    return () => {
      generation.current += 1;
      for (const timer of [noteTimer, runTimer]) {
        if (timer.current !== null) clearTimeout(timer.current);
        timer.current = null;
      }
      const id = refreshId.current;
      refreshId.current = null;
      if (id) void cancelRequest(id).catch(() => {});
    };
  }, [load]);

  // While a build runs the status is read again every few seconds.
  const building = status?.building ?? false;
  useEffect(() => {
    if (!building) return;
    const timer = setInterval(load, CACHE_POLL_MS);
    return () => clearInterval(timer);
  }, [building, load]);

  const refresh = useCallback(() => {
    if (sessionId === null || refreshId.current !== null) return;
    const mine = generation.current;
    const requestId = newRequestId();
    refreshId.current = requestId;
    setRefreshing(true);
    setError(null);
    const scope: RefreshScope = { type: "full" };
    schemaCacheRefresh(sessionId, database, scope, requestId)
      .then(
        (next) => {
          if (mine !== generation.current) return;
          setStatus(next);
          setError(null);
        },
        (failure) => {
          if (mine === generation.current) setError(toCommandError(failure));
        },
      )
      .finally(() => {
        if (refreshId.current === requestId) refreshId.current = null;
        if (mine === generation.current) setRefreshing(false);
      });
  }, [sessionId, database]);

  const noteCacheState = useCallback(
    (state: CacheState) => {
      if (state === known.current) return;
      const wait = lastLoad.current + NOTE_INTERVAL_MS - Date.now();
      if (wait <= 0) {
        load();
        return;
      }
      // Too soon after the last load: load once the window ends (repeated notes coalesce).
      if (noteTimer.current !== null) return;
      noteTimer.current = setTimeout(() => {
        noteTimer.current = null;
        load();
      }, wait);
    },
    [load],
  );

  const noteRunFinished = useCallback(() => {
    if (sessionId === null) return;
    if (runTimer.current !== null) clearTimeout(runTimer.current);
    runTimer.current = setTimeout(() => {
      runTimer.current = null;
      load();
    }, RUN_SETTLE_MS);
  }, [sessionId, load]);

  return { status, error, refreshing, refresh, noteCacheState, noteRunFinished };
}
