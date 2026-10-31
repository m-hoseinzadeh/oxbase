import { CompactSelection, type GridSelection } from "@glideapps/glide-data-grid";
import { describe, expect, it } from "vitest";
import {
  fromGridSelection,
  NO_ROWS,
  normalizeRanges,
  selectedRowCount,
  selectedRows,
  singleRow,
  toGridSelection,
} from "./rowSelection";

function markers(...rows: number[]): GridSelection {
  return { columns: CompactSelection.empty(), rows: CompactSelection.fromArray(rows) };
}

describe("row selection", () => {
  it("keeps the gaps of a Ctrl/Cmd-click row selection", () => {
    const selection = fromGridSelection(markers(1, 4999));
    expect(selection).toEqual({
      current: 1,
      ranges: [
        [1, 2],
        [4999, 5000],
      ],
    });
    expect(selectedRowCount(selection, 6000)).toBe(2);
    expect(selectedRows(selection, 6000)).toEqual([1, 4999]);
  });

  it("reads a cell range and its stack", () => {
    const selection = fromGridSelection({
      columns: CompactSelection.empty(),
      rows: CompactSelection.empty(),
      current: {
        cell: [2, 5],
        range: { x: 2, y: 5, width: 1, height: 3 },
        rangeStack: [{ x: 0, y: 10, width: 2, height: 1 }],
      },
    });
    expect(selection).toEqual({
      current: 5,
      ranges: [
        [5, 8],
        [10, 11],
      ],
    });
  });

  it("merges and clips", () => {
    expect(
      normalizeRanges([
        [4, 6],
        [0, 2],
        [2, 3],
        [5, 9],
        [7, 7],
      ]),
    ).toEqual([
      [0, 3],
      [4, 9],
    ]);
    const selection = { current: null, ranges: [[0, 3] as const, [8, 20] as const] };
    expect(selectedRowCount(selection, 10)).toBe(5);
    expect(selectedRows(selection, 10, 4)).toEqual([0, 1, 2, 8]);
    expect(selectedRowCount(NO_ROWS, 10)).toBe(0);
    expect(selectedRows({ current: 3, ranges: [] }, 10)).toEqual([3]);
  });

  it("turns a held selection back into a Glide selection", () => {
    const one = toGridSelection(singleRow(7), 3);
    expect(one.current?.cell).toEqual([3, 7]);
    expect(one.rows.length).toBe(0);
    const gaps = toGridSelection({
      current: 1,
      ranges: [
        [1, 2],
        [4, 6],
      ],
    });
    expect(gaps.current).toBeUndefined();
    expect(gaps.rows.toArray()).toEqual([1, 4, 5]);
    expect(fromGridSelection(gaps).ranges).toEqual([
      [1, 2],
      [4, 6],
    ]);
  });
});
