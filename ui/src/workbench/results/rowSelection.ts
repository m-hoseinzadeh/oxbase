/**
 * The grid's row selection as the panels see it: the current row plus the exact selected rows
 * as ranges. A selection with gaps (Ctrl/Cmd-click on row markers) stays a selection with gaps;
 * it is never widened to its first..last span.
 */
import { CompactSelection, type GridSelection } from "@glideapps/glide-data-grid";

/** A run of selected rows: `[start, end)`. */
export type RowRange = readonly [number, number];

export interface GridRowSelection {
  /** The selected cell's row (or the first selected row); `null` = nothing selected. */
  current: number | null;
  /** The selected rows: ascending, non-overlapping, non-adjacent `[start, end)` ranges. */
  ranges: readonly RowRange[];
}

export const NO_ROWS: GridRowSelection = { current: null, ranges: [] };

/** One selected row. */
export function singleRow(row: number): GridRowSelection {
  return { current: row, ranges: [[row, row + 1]] };
}

/** Sorts and merges overlapping or adjacent ranges; drops empty ones. */
export function normalizeRanges(ranges: readonly RowRange[]): RowRange[] {
  const sorted = ranges.filter(([a, b]) => b > a).sort((x, y) => x[0] - y[0]);
  const out: [number, number][] = [];
  for (const [a, b] of sorted) {
    const last = out[out.length - 1];
    if (last && a <= last[1]) last[1] = Math.max(last[1], b);
    else out.push([a, b]);
  }
  return out;
}

/** The ranges cut to rows `0..total-1`. */
export function clipRanges(ranges: readonly RowRange[], total: number): RowRange[] {
  return ranges.flatMap(([a, b]): RowRange[] => {
    const start = Math.max(0, a);
    const end = Math.min(total, b);
    return end > start ? [[start, end]] : [];
  });
}

/** How many rows of `0..total-1` are selected (the current row alone counts as one). */
export function selectedRowCount(selection: GridRowSelection, total: number): number {
  const n = clipRanges(selection.ranges, total).reduce((sum, [a, b]) => sum + (b - a), 0);
  if (n > 0) return n;
  return selection.current !== null && selection.current < total ? 1 : 0;
}

/** The selected rows of `0..total-1`, ascending, at most `limit` of them. */
export function selectedRows(
  selection: GridRowSelection,
  total: number,
  limit = Number.POSITIVE_INFINITY,
): number[] {
  const rows: number[] = [];
  for (const [a, b] of clipRanges(selection.ranges, total)) {
    for (let r = a; r < b && rows.length < limit; r += 1) rows.push(r);
    if (rows.length >= limit) break;
  }
  if (rows.length === 0 && selection.current !== null && selection.current < total && limit > 0) {
    rows.push(selection.current);
  }
  return rows;
}

/** The exact rows of a Glide selection: selected row markers, the cell range and its stack. */
export function fromGridSelection(selection: GridSelection): GridRowSelection {
  const ranges: RowRange[] = [...selection.rows.items];
  const current = selection.current;
  if (current) {
    for (const rect of [current.range, ...current.rangeStack]) {
      ranges.push([rect.y, rect.y + Math.max(rect.height, 1)]);
    }
  }
  const merged = normalizeRanges(ranges);
  return { current: current ? current.cell[1] : (merged[0]?.[0] ?? null), ranges: merged };
}

/**
 * A Glide selection showing `selection`: one row is a cell selection in column `col`, more
 * rows (or a row without a current cell) are row-marker selections.
 */
export function toGridSelection(selection: GridRowSelection, col = 0): GridSelection {
  const columns = CompactSelection.empty();
  const ranges = normalizeRanges(selection.ranges);
  const only = ranges.length === 1 ? ranges[0] : undefined;
  const row = selection.current;
  if (row !== null && (ranges.length === 0 || (only && only[0] === row && only[1] === row + 1))) {
    return {
      columns,
      rows: CompactSelection.empty(),
      current: {
        cell: [col, row],
        range: { x: col, y: row, width: 1, height: 1 },
        rangeStack: [],
      },
    };
  }
  let rows = CompactSelection.empty();
  for (const range of ranges) rows = rows.add([range[0], range[1]]);
  return { columns, rows };
}
