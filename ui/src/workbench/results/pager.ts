/**
 * The grid's data source for one result set: rows come from the core page by page
 * (`query_page`, Arrow IPC), never all at once.
 *
 * - Only the visible rows (plus `prefetchPages` around them) are requested.
 * - Requests are coalesced: scroll events only record the wanted range; at most one request is
 *   in flight, and the next one is computed from the latest range once the throttle window
 *   (`coalesceMs`) has passed. A burst of scroll events therefore costs a few requests, and
 *   pages scrolled past in between are never fetched.
 * - Contiguous missing pages go out as one request (up to `maxPagesPerRequest` pages).
 * - The cache keeps at most `maxPages` pages and at most `maxBytes` of decoded Arrow data (least
 *   recently used pages are dropped, never the visible ones), so memory stays bounded however
 *   far the user scrolls and however wide the rows are.
 * - Requests are sized by bytes too: once the rows turn out wide, fewer pages go per request
 *   (down to one) and no pages are prefetched past `maxRequestBytes`.
 * - While the result is still streaming, the row count grows; a page that came back short is
 *   fetched again once more rows exist.
 */
import type { Table, Vector } from "apache-arrow";

export type PageFetcher = (offset: number, limit: number) => Promise<Table>;

export interface PagerOptions {
  /** Rows per cached page. */
  pageSize?: number;
  /** Most pages kept in memory. */
  maxPages?: number;
  /** Most pages fetched by one request. */
  maxPagesPerRequest?: number;
  /** Pages fetched ahead and behind the visible ones. */
  prefetchPages?: number;
  /** Throttle window between requests, in ms. */
  coalesceMs?: number;
  /** Most decoded bytes kept in memory (visible pages are kept even past it). */
  maxBytes?: number;
  /** Aimed-for size of one response, in decoded bytes. */
  maxRequestBytes?: number;
}

/** One cached page. `rows` can be short of `pageSize` at the end or while streaming. */
interface Page {
  table: Table;
  rows: number;
  /** The row count when the page was fetched: a short page is refetched only once it grew. */
  knownTotal: number;
  /** Decoded size: the page's share of its response's Arrow buffers. */
  bytes: number;
}

/** A cell of a loaded row: the Arrow column and the index inside it. */
export interface CellRef {
  vector: Vector;
  index: number;
}

/** Rows `[first, last]` were (re)loaded. */
export type LoadListener = (first: number, last: number) => void;

const DEFAULTS: Required<PagerOptions> = {
  pageSize: 200,
  maxPages: 60,
  maxPagesPerRequest: 16,
  prefetchPages: 1,
  coalesceMs: 24,
  maxBytes: 192 * 1024 * 1024,
  maxRequestBytes: 24 * 1024 * 1024,
};

/** Bytes of the Arrow buffers behind `table`. */
export function tableBytes(table: Table): number {
  let bytes = 0;
  for (const data of table.data) bytes += data.byteLength;
  return bytes;
}

export class ResultPager {
  readonly pageSize: number;
  readonly maxPages: number;
  private readonly maxPagesPerRequest: number;
  private readonly prefetchPages: number;
  private readonly coalesceMs: number;
  private readonly maxBytes: number;
  private readonly maxRequestBytes: number;

  private total = 0;
  /** Bytes held by the cached pages. */
  private bytes = 0;
  /** Bytes per row seen in the last response (0 until one arrived). */
  private rowBytes = 0;
  private readonly pages = new Map<number, Page>();
  private readonly loading = new Set<number>();
  private firstVisible = 0;
  private lastVisible = -1;
  private timer: ReturnType<typeof setTimeout> | null = null;
  private inFlight = false;
  private disposed = false;
  private readonly listeners = new Set<LoadListener>();

  /** Requests sent so far (for tests and diagnostics). */
  requests = 0;
  /** The last page request's error, cleared by the next successful one. */
  error: string | null = null;

  constructor(
    private readonly fetchPage: PageFetcher,
    options: PagerOptions = {},
  ) {
    const o = { ...DEFAULTS, ...options };
    this.pageSize = Math.max(1, Math.floor(o.pageSize));
    this.prefetchPages = Math.max(0, Math.floor(o.prefetchPages));
    this.maxPagesPerRequest = Math.max(1, Math.floor(o.maxPagesPerRequest));
    this.maxPages = Math.max(Math.floor(o.maxPages), 2 * this.prefetchPages + 2);
    this.coalesceMs = Math.max(0, o.coalesceMs);
    this.maxBytes = Math.max(1, o.maxBytes);
    this.maxRequestBytes = Math.max(1, o.maxRequestBytes);
  }

  get rowCount(): number {
    return this.total;
  }

  /** Pages currently held in memory. */
  get cachedPages(): number {
    return this.pages.size;
  }

  /** Decoded bytes currently held in memory. */
  get cachedBytes(): number {
    return this.bytes;
  }

  onLoad(listener: LoadListener): () => void {
    this.listeners.add(listener);
    return () => this.listeners.delete(listener);
  }

  /** The row count reported by the run (it only grows while the result streams). */
  setRowCount(total: number): void {
    if (this.disposed || total <= this.total) return;
    this.total = total;
    if (this.lastVisible >= this.firstVisible) this.schedule();
  }

  /** The grid shows rows `[first, last]` (inclusive). */
  setVisible(first: number, last: number): void {
    if (this.disposed) return;
    const f = Math.max(0, Math.floor(first));
    const l = Math.max(f - 1, Math.floor(last));
    if (f === this.firstVisible && l === this.lastVisible && this.error === null) return;
    this.firstVisible = f;
    this.lastVisible = l;
    // Visible pages become the most recently used.
    for (let p = this.pageOf(f); p <= this.pageOf(l); p += 1) {
      const page = this.pages.get(p);
      if (page) {
        this.pages.delete(p);
        this.pages.set(p, page);
      }
    }
    this.schedule();
  }

  /** The cell at `row`/`col`, or `null` while its page is not loaded. */
  cell(row: number, col: number): CellRef | null {
    const page = this.pages.get(this.pageOf(row));
    if (!page) return null;
    const index = row - this.pageOf(row) * this.pageSize;
    if (index >= page.rows) return null;
    const vector = page.table.getChildAt(col);
    return vector ? { vector, index } : null;
  }

  /**
   * Rows `[offset, offset + limit)` straight from the core, bypassing the cache (for copying a
   * selection: it neither evicts the visible pages nor stays in memory).
   */
  readRows(offset: number, limit: number): Promise<Table> {
    return this.fetchPage(offset, limit);
  }

  /** Drops every cached page (e.g. while the result's tab is hidden). */
  release(): void {
    this.pages.clear();
    this.bytes = 0;
    this.firstVisible = 0;
    this.lastVisible = -1;
  }

  /** Stops all work; later responses are ignored. */
  dispose(): void {
    this.disposed = true;
    if (this.timer !== null) clearTimeout(this.timer);
    this.timer = null;
    this.pages.clear();
    this.bytes = 0;
    this.listeners.clear();
  }

  private pageOf(row: number): number {
    return Math.floor(row / this.pageSize);
  }

  private schedule(): void {
    if (this.disposed || this.timer !== null || this.inFlight) return;
    this.timer = setTimeout(() => {
      this.timer = null;
      void this.flush();
    }, this.coalesceMs);
  }

  private needsFetch(p: number): boolean {
    if (this.loading.has(p) || p * this.pageSize >= this.total) return false;
    const page = this.pages.get(p);
    if (!page) return true;
    const end = p * this.pageSize + page.rows;
    return page.rows < this.pageSize && end < this.total && this.total > page.knownTotal;
  }

  /** Pages one request may carry: fewer once rows turn out wide, at least one. */
  private pagesPerRequest(): number {
    if (this.rowBytes === 0) return this.maxPagesPerRequest;
    const pages = Math.floor(this.maxRequestBytes / (this.rowBytes * this.pageSize));
    return Math.min(this.maxPagesPerRequest, Math.max(1, pages));
  }

  /** Pages to fetch, visible ones first, then the prefetch margin (nearest first). */
  private neededPages(): number[] {
    if (this.lastVisible < this.firstVisible || this.total === 0) return [];
    const first = this.pageOf(this.firstVisible);
    const last = this.pageOf(Math.min(this.lastVisible, this.total - 1));
    const out: number[] = [];
    for (let p = first; p <= last; p += 1) if (this.needsFetch(p)) out.push(p);
    // Pages too wide to fit a request are not prefetched.
    const prefetch = this.rowBytes * this.pageSize > this.maxRequestBytes ? 0 : this.prefetchPages;
    for (let d = 1; d <= prefetch; d += 1) {
      if (this.needsFetch(last + d)) out.push(last + d);
      if (first - d >= 0 && this.needsFetch(first - d)) out.push(first - d);
    }
    return out;
  }

  private async flush(): Promise<void> {
    if (this.inFlight || this.disposed) return;
    const needed = this.neededPages();
    const seed = needed[0];
    if (seed === undefined) return;
    // One request for the first needed page and the needed pages next to it.
    const wanted = new Set(needed);
    let start = seed;
    let end = seed;
    const most = this.pagesPerRequest();
    while (end - start + 1 < most && wanted.has(end + 1)) end += 1;
    while (end - start + 1 < most && wanted.has(start - 1)) start -= 1;
    const offset = start * this.pageSize;
    const limit = Math.min((end - start + 1) * this.pageSize, this.total - offset);
    const knownTotal = this.total;
    for (let p = start; p <= end; p += 1) this.loading.add(p);
    this.inFlight = true;
    this.requests += 1;
    let loadedRows = 0;
    let failed = false;
    try {
      const table = await this.fetchPage(offset, limit);
      if (this.disposed) return;
      loadedRows = table.numRows;
      const responseBytes = tableBytes(table);
      if (loadedRows > 0) this.rowBytes = responseBytes / loadedRows;
      for (let p = start; p <= end; p += 1) {
        const from = (p - start) * this.pageSize;
        const to = Math.min(from + this.pageSize, table.numRows);
        const slice = table.slice(from, Math.max(from, to));
        const bytes = loadedRows > 0 ? (responseBytes * slice.numRows) / loadedRows : 0;
        this.dropPage(p);
        this.pages.set(p, { table: slice, rows: slice.numRows, knownTotal, bytes });
        this.bytes += bytes;
      }
      this.error = null;
    } catch (error) {
      if (this.disposed) return;
      failed = true;
      this.error = error instanceof Error ? error.message : String(error);
    } finally {
      for (let p = start; p <= end; p += 1) this.loading.delete(p);
      this.inFlight = false;
    }
    this.evict();
    for (const listener of this.listeners) listener(offset, offset + Math.max(loadedRows, 1) - 1);
    // A failed request is retried on the next scroll or row count change, not in a loop.
    if (!failed) this.schedule();
  }

  private dropPage(p: number): void {
    const page = this.pages.get(p);
    if (!page) return;
    this.pages.delete(p);
    this.bytes -= page.bytes;
  }

  private overBudget(): boolean {
    return this.pages.size > this.maxPages || this.bytes > this.maxBytes;
  }

  /**
   * Drops the least recently used pages beyond `maxPages` or `maxBytes`, never a visible one.
   */
  private evict(): void {
    if (!this.overBudget()) return;
    const firstKeep = this.pageOf(this.firstVisible);
    const lastKeep = this.pageOf(Math.max(this.lastVisible, this.firstVisible));
    for (const p of [...this.pages.keys()]) {
      if (!this.overBudget()) break;
      if (p >= firstKeep && p <= lastKeep) continue;
      this.dropPage(p);
    }
    if (this.pages.size === 0) this.bytes = 0;
  }
}
