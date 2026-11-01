/**
 * Glide Data Grid uses two column numberings. Callbacks such as `getCellContent` and
 * `onGridSelectionChange` see data columns (0 = the first result column), but the overlay
 * editor's `provideEditor` gets the cell's location in grid columns, which count the row-marker
 * column too. The result grid always shows row numbers, so the two differ by one.
 */
import type { Item } from "@glideapps/glide-data-grid";

/** Grid columns before the first data column (`rowMarkers="number"`). */
export const ROW_MARKER_COLUMNS = 1;

/** The data cell of a location Glide reports in grid columns (the overlay editor's). */
export function dataCellOf(location: Item): Item | null {
  const col = location[0] - ROW_MARKER_COLUMNS;
  return col < 0 ? null : [col, location[1]];
}
