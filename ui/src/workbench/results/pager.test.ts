import { Table, tableFromArrays, Utf8, vectorFromArray } from "apache-arrow";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { ResultPager } from "./pager";

const MILLION = 1_000_000;

/** A mocked `query_page` over a result of `rows()` rows: row i has id i. */
function backend(rows: () => number) {
  const calls: Array<{ offset: number; limit: number }> = [];
  const fetch = vi.fn(async (offset: number, limit: number) => {
    calls.push({ offset, limit });
    const n = Math.max(0, Math.min(limit, rows() - offset));
    return tableFromArrays({ id: Float64Array.from({ length: n }, (_, i) => offset + i) });
  });
  return { fetch, calls };
}

/** Lets the timers and the pending fetch promises run. */
async function settle(ms = 50) {
  for (let i = 0; i < 10; i += 1) await vi.advanceTimersByTimeAsync(ms);
}

function idAt(pager: ResultPager, row: number): unknown {
  const cell = pager.cell(row, 0);
  return cell ? cell.vector.get(cell.index) : undefined;
}

describe("ResultPager", () => {
  beforeEach(() => {
    vi.useFakeTimers();
  });
  afterEach(() => {
    vi.useRealTimers();
  });

  it("fetches only the visible pages of a 1,000,000-row result", async () => {
    const { fetch, calls } = backend(() => MILLION);
    const pager = new ResultPager(fetch, { pageSize: 200, prefetchPages: 1 });
    pager.setRowCount(MILLION);
    pager.setVisible(0, 39);
    await settle();
    expect(idAt(pager, 0)).toBe(0);
    expect(idAt(pager, 39)).toBe(39);
    // The visible page and one page of prefetch, in one request.
    expect(calls).toEqual([{ offset: 0, limit: 400 }]);

    pager.setVisible(500_000, 500_039);
    await settle();
    expect(idAt(pager, 500_010)).toBe(500_010);
    for (const call of calls.slice(1)) {
      expect(call.offset).toBeGreaterThanOrEqual(500_000 - 200);
      expect(call.offset + call.limit).toBeLessThanOrEqual(500_040 + 400);
    }
    const fetched = calls.reduce((sum, c) => sum + c.limit, 0);
    expect(fetched).toBeLessThan(2_000);
  });

  it("shows nothing for rows not loaded yet", () => {
    const { fetch } = backend(() => MILLION);
    const pager = new ResultPager(fetch);
    pager.setRowCount(MILLION);
    expect(pager.cell(123_456, 0)).toBeNull();
    expect(fetch).not.toHaveBeenCalled();
  });

  it("coalesces a burst of 100 scroll events into a few requests", async () => {
    let resolveSlow: (() => void) | null = null;
    const { fetch: fast } = backend(() => MILLION);
    // A backend that answers only after 40 ms, like a real page read.
    const fetch = vi.fn(
      (offset: number, limit: number) =>
        new Promise<Awaited<ReturnType<typeof fast>>>((resolve) => {
          resolveSlow = () => void fast(offset, limit).then(resolve);
          setTimeout(() => resolveSlow?.(), 40);
        }),
    );
    const pager = new ResultPager(fetch, { pageSize: 100, coalesceMs: 16 });
    pager.setRowCount(MILLION);
    // 100 scroll events, 2 ms apart, 1,000 rows each: a fast drag over 100,000 rows.
    for (let i = 0; i < 100; i += 1) {
      pager.setVisible(i * 1000, i * 1000 + 39);
      await vi.advanceTimersByTimeAsync(2);
    }
    await settle();
    expect(fetch.mock.calls.length).toBeLessThanOrEqual(8);
    // The final position is loaded.
    expect(idAt(pager, 99_000)).toBe(99_000);
    expect(idAt(pager, 99_039)).toBe(99_039);
  });

  it("sends one request for a synchronous burst", async () => {
    const { fetch } = backend(() => MILLION);
    const pager = new ResultPager(fetch, { pageSize: 100 });
    pager.setRowCount(MILLION);
    for (let i = 0; i < 100; i += 1) pager.setVisible(i * 37, i * 37 + 40);
    await settle();
    expect(fetch).toHaveBeenCalledTimes(1);
    expect(idAt(pager, 99 * 37)).toBe(99 * 37);
  });

  it("caps the cached pages however far the user scrolls", async () => {
    const { fetch } = backend(() => MILLION);
    const pager = new ResultPager(fetch, { pageSize: 100, maxPages: 12 });
    pager.setRowCount(MILLION);
    let most = 0;
    for (let row = 0; row < 200_000; row += 2_500) {
      pager.setVisible(row, row + 49);
      await settle(20);
      most = Math.max(most, pager.cachedPages);
      expect(idAt(pager, row + 49)).toBe(row + 49);
    }
    expect(most).toBeLessThanOrEqual(12);
    // Pages scrolled away from are gone.
    expect(pager.cell(0, 0)).toBeNull();
  });

  it("caps the cached bytes and shrinks requests when rows are wide", async () => {
    const wide = "x".repeat(10_000); // about 10 kB a row
    const calls: Array<{ offset: number; limit: number }> = [];
    const fetch = vi.fn(async (offset: number, limit: number) => {
      calls.push({ offset, limit });
      const n = Math.max(0, Math.min(limit, MILLION - offset));
      return new Table({
        id: vectorFromArray(Float64Array.from({ length: n }, (_, i) => offset + i)),
        text: vectorFromArray(
          Array.from({ length: n }, () => wide),
          new Utf8(),
        ),
      });
    });
    const maxBytes = 8_000_000;
    const pager = new ResultPager(fetch, {
      pageSize: 100,
      maxPages: 1000,
      prefetchPages: 1,
      maxBytes,
      maxRequestBytes: 2_500_000,
    });
    pager.setRowCount(MILLION);
    pager.setVisible(0, 49);
    await settle();
    expect(idAt(pager, 49)).toBe(49);
    for (let row = 1000; row < 100_000; row += 1000) {
      pager.setVisible(row, row + 49);
      await settle(20);
      expect(idAt(pager, row + 49)).toBe(row + 49);
      // The visible page plus at most what the budget allows.
      expect(pager.cachedBytes).toBeLessThanOrEqual(maxBytes + 1_100_000);
    }
    // Once a 1 MB page is known, one request carries at most two pages (2.5 MB aimed for).
    for (const call of calls.slice(1)) expect(call.limit).toBeLessThanOrEqual(200);
    expect(pager.cachedPages).toBeLessThan(10);
  });

  it("reads rows for copying without touching the cache", async () => {
    const { fetch } = backend(() => MILLION);
    const pager = new ResultPager(fetch, { pageSize: 100 });
    pager.setRowCount(MILLION);
    const table = await pager.readRows(500_000, 3);
    expect(table.numRows).toBe(3);
    expect(pager.cachedPages).toBe(0);
  });

  it("follows a growing row count while the result streams", async () => {
    let arrived = 50;
    const { fetch, calls } = backend(() => arrived);
    const pager = new ResultPager(fetch, { pageSize: 200, prefetchPages: 0 });
    pager.setRowCount(arrived);
    pager.setVisible(0, 99);
    await settle();
    expect(idAt(pager, 49)).toBe(49);
    expect(pager.cell(50, 0)).toBeNull();

    // More rows arrive: the short page is fetched again.
    arrived = 180;
    pager.setRowCount(arrived);
    await settle();
    expect(idAt(pager, 99)).toBe(99);
    expect(calls.length).toBe(2);

    // No new rows, no new request.
    pager.setRowCount(arrived);
    await settle();
    expect(calls.length).toBe(2);
    expect(pager.rowCount).toBe(180);
  });

  it("does not loop on a failing page and retries on the next scroll", async () => {
    const { fetch: ok } = backend(() => 1000);
    const fetch = vi.fn(ok).mockRejectedValueOnce(new Error("run closed"));
    const pager = new ResultPager(fetch, { pageSize: 100 });
    const loaded = vi.fn();
    pager.onLoad(loaded);
    pager.setRowCount(1000);
    pager.setVisible(0, 49);
    await settle();
    expect(pager.error).toBe("run closed");
    expect(fetch).toHaveBeenCalledTimes(1);
    pager.setVisible(0, 49);
    await settle();
    expect(pager.error).toBeNull();
    expect(idAt(pager, 10)).toBe(10);
    expect(loaded).toHaveBeenCalled();
  });

  it("ignores answers after dispose", async () => {
    const { fetch } = backend(() => 1000);
    const pager = new ResultPager(fetch);
    pager.setRowCount(1000);
    pager.setVisible(0, 10);
    pager.dispose();
    await settle();
    expect(fetch).not.toHaveBeenCalled();
    expect(pager.cachedPages).toBe(0);
  });
});
