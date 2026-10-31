/**
 * Page latency hook for the webview smoke test (docs/m3-contracts.md §9): every page the grid
 * fetches is timed (IPC round trip and Arrow decode separately) and recorded in
 * `window.__oxbasePerf.pages`. Cheap and always on; the list keeps the latest entries only.
 */
import type { Table } from "apache-arrow";
import type { RunId } from "../../api";
import { call } from "../../api/call";
import { decodeArrowPage, type RawBytes } from "../../ipc";

export interface PagePerf {
  offset: number;
  limit: number;
  ipcMs: number;
  decodeMs: number;
}

declare global {
  interface Window {
    __oxbasePerf?: { pages: PagePerf[] };
  }
}

/** Most entries kept. */
export const MAX_PERF_ENTRIES = 10_000;

export function perfLog(): PagePerf[] {
  if (!window.__oxbasePerf) window.__oxbasePerf = { pages: [] };
  return window.__oxbasePerf.pages;
}

export function recordPage(entry: PagePerf): void {
  const pages = perfLog();
  pages.push(entry);
  if (pages.length > MAX_PERF_ENTRIES) pages.splice(0, pages.length - MAX_PERF_ENTRIES);
}

/** `queryPage` with its IPC and decode times recorded. */
export async function timedQueryPage(
  runId: RunId,
  resultIndex: number,
  offset: number,
  limit: number,
): Promise<Table> {
  const start = performance.now();
  const raw = await call<RawBytes>("query_page", { runId, resultIndex, offset, limit });
  const received = performance.now();
  const table = decodeArrowPage(raw);
  const decoded = performance.now();
  recordPage({ offset, limit, ipcMs: received - start, decodeMs: decoded - received });
  return table;
}
