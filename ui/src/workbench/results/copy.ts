/**
 * The cells Glide copies to the clipboard (its `getCellsForSelection`). The grid's own fallback
 * walks `getCellContent` cell by cell, which on a large result freezes the UI thread and copies
 * "#LOADING" for every row whose page is not cached. Instead:
 *
 * - a small selection whose rows are all cached is answered at once from the cache;
 * - anything else is read from the core in chunks (`query_page`, bypassing the cache), up to
 *   {@link MAX_COPY_ROWS} rows and {@link MAX_COPY_CELLS} cells; a larger selection copies its
 *   first rows and says so.
 * - Copying selected rows (row markers) asks for one row at a time; those requests are batched
 *   into a few range reads, and more than {@link MAX_COPY_ROWS} rows are refused.
 *
 * Copied cells carry the full value (not the shortened one-line cell text), and NULL copies as
 * `NULL`, apart from an empty string.
 */
import { type GridCell, GridCellKind, type Rectangle } from "@glideapps/glide-data-grid";
import type { Table } from "apache-arrow";
import type { ColumnInfo } from "../../api";
import { type FormattedCell, formatCell } from "./format";
import type { ResultPager } from "./pager";

/** Most rows one copy reads. */
export const MAX_COPY_ROWS = 100_000;
/** Most cells one copy builds. */
export const MAX_COPY_CELLS = 1_000_000;
/** Rows per `query_page` request while copying. */
export const COPY_CHUNK_ROWS = 5_000;
/** Selections up to this many cells, all cached, are answered synchronously. */
const SYNC_CELLS = 10_000;

/** What NULL copies as. */
export const NULL_COPY_TEXT = "NULL";

export type CellArray = readonly (readonly GridCell[])[];
export type CopyNotify = (message: string) => void;

function copyCell(value: FormattedCell): GridCell {
  const text = value.isNull ? NULL_COPY_TEXT : value.text;
  return { kind: GridCellKind.Text, data: text, displayData: text, allowOverlay: false };
}

function plural(n: number, one: string, many: string): string {
  return `${n.toLocaleString("en-US")} ${n === 1 ? one : many}`;
}

/** The selection clipped to the result's columns and stored rows. */
function clip(rect: Rectangle, columns: number, rows: number): Rectangle {
  const x = Math.max(0, rect.x);
  const y = Math.max(0, rect.y);
  const width = Math.max(0, Math.min(rect.x + rect.width, columns) - x);
  const height = Math.max(0, Math.min(rect.y + rect.height, rows) - y);
  return { x, y, width, height };
}

function cachedCells(pager: ResultPager, columns: ColumnInfo[], r: Rectangle): CellArray | null {
  if (r.width * r.height > SYNC_CELLS) return null;
  for (let row = r.y; row < r.y + r.height; row += 1) {
    if (pager.cell(row, r.x) === null) return null;
  }
  const out: GridCell[][] = [];
  for (let row = r.y; row < r.y + r.height; row += 1) {
    const line: GridCell[] = [];
    for (let col = r.x; col < r.x + r.width; col += 1) {
      const ref = pager.cell(row, col);
      if (!ref) return null;
      line.push(copyCell(formatCell(ref.vector, ref.index, columns[col])));
    }
    out.push(line);
  }
  return out;
}

/** The cells of row `i` of `table`, columns `r.x .. r.x + r.width - 1`. */
function rowCells(table: Table, i: number, columns: ColumnInfo[], r: Rectangle): GridCell[] {
  const line: GridCell[] = [];
  for (let col = r.x; col < r.x + r.width; col += 1) {
    const vector = table.getChildAt(col);
    line.push(
      vector
        ? copyCell(formatCell(vector, i, columns[col]))
        : copyCell({ text: "", isNull: true, numeric: false }),
    );
  }
  return line;
}

function tableCells(table: Table, columns: ColumnInfo[], r: Rectangle, out: GridCell[][]): void {
  for (let i = 0; i < table.numRows; i += 1) out.push(rowCells(table, i, columns, r));
}

function errorText(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

function abortError(): Error {
  const error = new Error("Copy cancelled");
  error.name = "AbortError";
  return error;
}

async function readCells(
  pager: ResultPager,
  columns: ColumnInfo[],
  r: Rectangle,
  signal: AbortSignal,
  notify: CopyNotify,
): Promise<CellArray> {
  const maxRows = Math.min(r.height, MAX_COPY_ROWS, Math.floor(MAX_COPY_CELLS / r.width));
  const rows = Math.max(1, maxRows);
  const out: GridCell[][] = [];
  try {
    for (let offset = r.y; offset < r.y + rows; offset += COPY_CHUNK_ROWS) {
      if (signal.aborted) throw abortError();
      const limit = Math.min(COPY_CHUNK_ROWS, r.y + rows - offset);
      const table = await pager.readRows(offset, limit);
      tableCells(table, columns, r, out);
      if (table.numRows < limit) break;
    }
  } catch (error) {
    if (!signal.aborted) {
      notify(`Copy failed: ${errorText(error)}`);
    }
    // Throwing leaves the clipboard as it was, rather than with part of the selection.
    throw error;
  }
  if (signal.aborted) throw abortError();
  if (out.length < r.height) {
    notify(
      `Copied the first ${plural(out.length, "row", "rows")} of ${plural(r.height, "selected row", "selected rows")}`,
    );
  }
  return out;
}

/** Rows read in between two wanted rows rather than splitting a batched read. */
const MAX_ROW_GAP = 64;

interface RowRequest {
  r: Rectangle;
  resolve: (cells: CellArray) => void;
  reject: (error: unknown) => void;
}

interface RowBatch {
  requests: RowRequest[];
  columns: ColumnInfo[];
  signal: AbortSignal;
  notify: CopyNotify;
}

/** One-row requests made in the same task, per result (Glide asks row by row). */
const rowBatches = new WeakMap<ResultPager, RowBatch>();

function readRow(
  pager: ResultPager,
  columns: ColumnInfo[],
  r: Rectangle,
  signal: AbortSignal,
  notify: CopyNotify,
): Promise<CellArray> {
  return new Promise((resolve, reject) => {
    let batch = rowBatches.get(pager);
    if (!batch) {
      const created: RowBatch = { requests: [], columns, signal, notify };
      rowBatches.set(pager, created);
      queueMicrotask(() => {
        rowBatches.delete(pager);
        void flushRows(pager, created);
      });
      batch = created;
    }
    batch.requests.push({ r, resolve, reject });
  });
}

async function flushRows(pager: ResultPager, batch: RowBatch): Promise<void> {
  const { requests, columns, signal, notify } = batch;
  const fail = (error: unknown) => {
    for (const q of requests) q.reject(error);
  };
  const rows = [...new Set(requests.map((q) => q.r.y))].sort((a, b) => a - b);
  if (rows.length > MAX_COPY_ROWS) {
    const message = `Too many rows to copy: ${plural(rows.length, "row", "rows")} selected, at most ${MAX_COPY_ROWS.toLocaleString("en-US")}`;
    notify(message);
    fail(new Error(message));
    return;
  }
  const found = new Map<number, { table: Table; index: number }>();
  try {
    let i = 0;
    while (i < rows.length) {
      if (signal.aborted) throw abortError();
      const start = rows[i] ?? 0;
      let end = start;
      i += 1;
      while (i < rows.length) {
        const next = rows[i] ?? 0;
        if (next - end > MAX_ROW_GAP || next - start + 1 > COPY_CHUNK_ROWS) break;
        end = next;
        i += 1;
      }
      const table = await pager.readRows(start, end - start + 1);
      for (let k = 0; k < table.numRows; k += 1) found.set(start + k, { table, index: k });
    }
  } catch (error) {
    if (!signal.aborted) notify(`Copy failed: ${errorText(error)}`);
    fail(error);
    return;
  }
  for (const q of requests) {
    const hit = found.get(q.r.y);
    if (hit) q.resolve([rowCells(hit.table, hit.index, columns, q.r)]);
    else q.reject(new Error(`row ${q.r.y + 1} is not available`));
  }
}

/**
 * Glide's `getCellsForSelection`: the cells of `rect` (row-major), at once when they are
 * cached, else as a thunk that reads them from the core.
 */
export function cellsForSelection(
  pager: ResultPager,
  columns: ColumnInfo[],
  rowCount: number,
  rect: Rectangle,
  signal: AbortSignal,
  notify: CopyNotify,
): CellArray | (() => Promise<CellArray>) {
  const r = clip(rect, columns.length, rowCount);
  if (r.width === 0 || r.height === 0) return [];
  const cached = cachedCells(pager, columns, r);
  if (cached) return cached;
  if (r.height === 1) return () => readRow(pager, columns, r, signal, notify);
  return () => readCells(pager, columns, r, signal, notify);
}
