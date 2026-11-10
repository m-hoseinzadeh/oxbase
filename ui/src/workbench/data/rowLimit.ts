/**
 * How many rows a table-data tab loads (`TableDataQuery.limit`). Opening a table without a
 * limit streams every row, and the server keeps the SELECT's locks until the stream ends, so
 * tabs load the first {@link DEFAULT_ROW_LIMIT} rows unless the user asks for more. When the
 * rows are in, the tab gives its session back (see TableDataPanel). The default
 * for new tabs is set in the Settings window; a tab's Rows control changes that tab only.
 */

/** The choices of the Rows control; `null` = every row. */
export const ROW_LIMITS: readonly (number | null)[] = [100, 500, 1000, 5000, 10_000, 50_000, null];

export const DEFAULT_ROW_LIMIT = 1000;

const KEY = "oxbase.tableData.rowLimit";

/** The limit new table-data tabs start with. */
export function defaultRowLimit(): number | null {
  try {
    const stored = globalThis.localStorage?.getItem(KEY);
    if (stored === "all") return null;
    const n = stored ? Number(stored) : Number.NaN;
    if (ROW_LIMITS.includes(n)) return n;
  } catch {
    // Storage can be unavailable (private mode, tests): the default applies.
  }
  return DEFAULT_ROW_LIMIT;
}

/** Remembers `limit` as the default for new tabs. */
export function saveDefaultRowLimit(limit: number | null): void {
  try {
    globalThis.localStorage?.setItem(KEY, limit === null ? "all" : String(limit));
  } catch {
    // Not remembered; the choice still applies to this tab.
  }
}

/** "1,000 rows" / "All rows", for the control. */
export function rowLimitLabel(limit: number | null): string {
  return limit === null ? "All rows" : `${limit.toLocaleString("en-US")} rows`;
}

/** The value of the control's `<option>`s. */
export function rowLimitValue(limit: number | null): string {
  return limit === null ? "all" : String(limit);
}

export function parseRowLimit(value: string): number | null {
  return value === "all" ? null : Number(value);
}
