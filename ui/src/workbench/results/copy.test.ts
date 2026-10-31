import { type GridCell, GridCellKind } from "@glideapps/glide-data-grid";
import { Float64, Table, Utf8, vectorFromArray } from "apache-arrow";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { ColumnInfo } from "../../api";
import {
  type CellArray,
  COPY_CHUNK_ROWS,
  cellsForSelection,
  MAX_COPY_ROWS,
  NULL_COPY_TEXT,
} from "./copy";
import { ResultPager } from "./pager";

const MILLION = 1_000_000;

const COLUMNS: ColumnInfo[] = [
  { name: "id", dataType: "Float64", nullable: false },
  { name: "note", dataType: "Utf8", nullable: true },
];

/** A mocked `query_page` over 1,000,000 rows: id i, note "line i\nnext" (NULL every 3rd). */
function backend() {
  return vi.fn(async (offset: number, limit: number) => {
    const n = Math.max(0, Math.min(limit, MILLION - offset));
    const ids = Array.from({ length: n }, (_, i) => offset + i);
    return new Table({
      id: vectorFromArray(ids, new Float64()),
      note: vectorFromArray(
        ids.map((id) => (id % 3 === 0 ? null : `line ${id}\nnext`)),
        new Utf8(),
      ),
    });
  });
}

function text(cell: GridCell | undefined): string {
  expect(cell?.kind).toBe(GridCellKind.Text);
  return cell && cell.kind === GridCellKind.Text ? cell.data : "";
}

async function resolve(result: CellArray | (() => Promise<CellArray>)): Promise<CellArray> {
  return typeof result === "function" ? result() : result;
}

describe("cellsForSelection", () => {
  beforeEach(() => {
    vi.useFakeTimers();
  });
  afterEach(() => {
    vi.useRealTimers();
  });

  it("answers a small cached selection at once, with full values and NULL", async () => {
    const fetch = backend();
    const pager = new ResultPager(fetch, { pageSize: 100 });
    pager.setRowCount(MILLION);
    pager.setVisible(0, 49);
    for (let i = 0; i < 10; i += 1) await vi.advanceTimersByTimeAsync(30);
    const calls = fetch.mock.calls.length;
    const notify = vi.fn();
    const cells = cellsForSelection(
      pager,
      COLUMNS,
      MILLION,
      { x: 0, y: 0, width: 2, height: 3 },
      new AbortController().signal,
      notify,
    );
    expect(typeof cells).not.toBe("function");
    const rows = cells as CellArray;
    expect(rows.map((r) => r.map(text))).toEqual([
      ["0", NULL_COPY_TEXT],
      ["1", "line 1\nnext"],
      ["2", "line 2\nnext"],
    ]);
    expect(fetch.mock.calls.length).toBe(calls);
    expect(notify).not.toHaveBeenCalled();
  });

  it("reads an uncached selection from the core, never as loading cells", async () => {
    const fetch = backend();
    const pager = new ResultPager(fetch, { pageSize: 100 });
    pager.setRowCount(MILLION);
    const notify = vi.fn();
    const result = cellsForSelection(
      pager,
      COLUMNS,
      MILLION,
      { x: 1, y: 500_000, width: 1, height: 12_000 },
      new AbortController().signal,
      notify,
    );
    expect(typeof result).toBe("function");
    const rows = await resolve(result);
    expect(rows).toHaveLength(12_000);
    expect(rows.every((r) => r[0]?.kind === GridCellKind.Text)).toBe(true);
    expect(text(rows[1]?.[0])).toBe(NULL_COPY_TEXT); // 500001 is a multiple of 3
    expect(text(rows[2]?.[0])).toBe("line 500002\nnext");
    // Chunked requests, and nothing kept in the grid's cache.
    expect(fetch.mock.calls.length).toBe(Math.ceil(12_000 / COPY_CHUNK_ROWS));
    expect(pager.cachedPages).toBe(0);
    expect(notify).not.toHaveBeenCalled();
  });

  it("caps a whole-column copy of a 1,000,000-row result and says so", async () => {
    const fetch = backend();
    const pager = new ResultPager(fetch, { pageSize: 100 });
    pager.setRowCount(MILLION);
    const notify = vi.fn();
    const rows = await resolve(
      cellsForSelection(
        pager,
        COLUMNS,
        MILLION,
        { x: 0, y: 0, width: 2, height: MILLION },
        new AbortController().signal,
        notify,
      ),
    );
    expect(rows).toHaveLength(MAX_COPY_ROWS);
    const asked = fetch.mock.calls.reduce((sum, [, limit]) => sum + limit, 0);
    expect(asked).toBe(MAX_COPY_ROWS);
    expect(notify).toHaveBeenCalledWith("Copied the first 100,000 rows of 1,000,000 selected rows");
  });

  it("batches row-by-row requests (selected rows) into a few reads", async () => {
    const fetch = backend();
    const pager = new ResultPager(fetch, { pageSize: 100 });
    pager.setRowCount(MILLION);
    const signal = new AbortController().signal;
    const selected = [
      ...Array.from({ length: 1000 }, (_, i) => i),
      ...Array.from({ length: 2000 }, (_, i) => 500_000 + i),
    ];
    // As Glide copies selected rows: one single-row call each, all in one task.
    const rows = await Promise.all(
      selected.map((y) =>
        resolve(
          cellsForSelection(
            pager,
            COLUMNS,
            MILLION,
            { x: 0, y, width: 2, height: 1 },
            signal,
            vi.fn(),
          ),
        ).then((r) => r[0]),
      ),
    );
    expect(rows).toHaveLength(3000);
    expect(text(rows[2500]?.[1])).toBe("line 501500\nnext");
    expect(fetch.mock.calls).toEqual([
      [0, 1000],
      [500_000, 2000],
    ]);
  });

  it("refuses to copy more selected rows than the cap", async () => {
    const fetch = backend();
    const pager = new ResultPager(fetch, { pageSize: 100 });
    pager.setRowCount(MILLION);
    const signal = new AbortController().signal;
    const notify = vi.fn();
    const all = Array.from({ length: MAX_COPY_ROWS + 1 }, (_, y) =>
      resolve(
        cellsForSelection(
          pager,
          COLUMNS,
          MILLION,
          { x: 0, y, width: 1, height: 1 },
          signal,
          notify,
        ),
      ),
    );
    await expect(Promise.all(all)).rejects.toThrow(/Too many rows to copy/);
    expect(fetch).not.toHaveBeenCalled();
    expect(notify).toHaveBeenCalledTimes(1);
  });

  it("stops when the copy is aborted", async () => {
    const fetch = backend();
    const pager = new ResultPager(fetch, { pageSize: 100 });
    pager.setRowCount(MILLION);
    const abort = new AbortController();
    const result = cellsForSelection(
      pager,
      COLUMNS,
      MILLION,
      { x: 0, y: 0, width: 1, height: 50_000 },
      abort.signal,
      vi.fn(),
    );
    abort.abort();
    await expect(resolve(result)).rejects.toThrow("Copy cancelled");
    expect(fetch).not.toHaveBeenCalled();
  });

  it("reports a failed read and keeps the clipboard", async () => {
    const pager = new ResultPager(vi.fn().mockRejectedValue(new Error("run closed")));
    pager.setRowCount(MILLION);
    const notify = vi.fn();
    const result = cellsForSelection(
      pager,
      COLUMNS,
      MILLION,
      { x: 0, y: 0, width: 1, height: 10 },
      new AbortController().signal,
      notify,
    );
    await expect(resolve(result)).rejects.toThrow("run closed");
    expect(notify).toHaveBeenCalledWith("Copy failed: run closed");
  });
});
