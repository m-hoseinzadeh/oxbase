/**
 * The editable grid's view model (pure): how stored rows, applied changes and the pending
 * buffer (`ui/src/api/pending.ts`) combine into what the grid shows, and how an `edit_apply`
 * report folds back (docs/m3-contracts.md §5).
 *
 * Grid rows are, in order: the stored rows of the result (`stored`), rows inserted by earlier
 * applies (numbered after the stored ones by the core), then the pending new rows of the buffer.
 */
import {
  type ApplyReport,
  type CellEdit,
  type CellInput,
  type EditColumn,
  type EditStatus,
  emptyChanges,
  type PendingChanges,
  type PendingEdit,
} from "../../api";

/** What earlier applies changed (the core overlays the same values on its result store). */
export interface AppliedState {
  /** Row number → the row read back after the apply (edit texts in result-column order). */
  values: ReadonlyMap<number, readonly (string | null)[]>;
  /** Rows deleted by an apply (they stay in the store but refuse edits). */
  deleted: ReadonlySet<number>;
  /** Applied inserts numbered past the stored rows. */
  extraRows: number;
}

export const NOTHING_APPLIED: AppliedState = {
  values: new Map(),
  deleted: new Set(),
  extraRows: 0,
};

export type GridRow = { type: "row"; row: number } | { type: "new"; edit: number; ordinal: number };

/** Edit indexes of the pending new rows, in buffer order. */
export function insertIndexes(changes: PendingChanges): number[] {
  const out: number[] = [];
  changes.edits.forEach((e, i) => {
    if (e.op === "insert") out.push(i);
  });
  return out;
}

/** Rows the grid shows. */
export function gridRowCount(
  stored: number,
  applied: AppliedState,
  changes: PendingChanges,
): number {
  return stored + applied.extraRows + insertIndexes(changes).length;
}

/** What grid row `index` is. */
export function gridRow(
  index: number,
  stored: number,
  applied: AppliedState,
  changes: PendingChanges,
): GridRow | null {
  const existing = stored + applied.extraRows;
  if (index < existing) return { type: "row", row: index };
  const inserts = insertIndexes(changes);
  const edit = inserts[index - existing];
  return edit === undefined ? null : { type: "new", edit, ordinal: index - existing + 1 };
}

/** The grid row of a pending new row's edit index. */
export function gridIndexOfInsert(
  edit: number,
  stored: number,
  applied: AppliedState,
  changes: PendingChanges,
): number {
  const at = insertIndexes(changes).indexOf(edit);
  return at < 0 ? -1 : stored + applied.extraRows + at;
}

/** The pending edit of an existing row, if any. */
export function editOfRow(changes: PendingChanges, row: number): PendingEdit | undefined {
  return changes.edits.find((e) => e.op !== "insert" && e.row === row);
}

/** The pending input of one cell (update or new row), if any. */
export function pendingInput(
  changes: PendingChanges,
  row: GridRow,
  column: string,
): CellInput | undefined {
  const edit = row.type === "new" ? changes.edits[row.edit] : editOfRow(changes, row.row);
  if (!edit || edit.op === "delete") return undefined;
  return edit.cells.find((c) => c.column === column)?.input;
}

export type RowStatus = "inserted" | "updated" | "deleted" | "removed" | null;

/** How a row is highlighted: pending insert / update / delete, or deleted by an apply. */
export function rowStatus(changes: PendingChanges, applied: AppliedState, row: GridRow): RowStatus {
  if (row.type === "new") return "inserted";
  if (applied.deleted.has(row.row)) return "removed";
  const edit = editOfRow(changes, row.row);
  if (edit?.op === "delete") return "deleted";
  if (edit?.op === "update") return "updated";
  return null;
}

/** A problem reported for one edit of the last failed apply. */
export interface EditProblem {
  /** The edit as it was sent (matched by row, or by identity for new rows). */
  edit: PendingEdit;
  index: number;
  status: EditStatus;
  message: string;
}

const STATUS_TEXT: Record<EditStatus, string> = {
  applied: "Applied",
  conflict:
    "Conflict: the row was changed or deleted since it was loaded (not exactly one row matched)",
  failed: "Failed",
  rolledBack: "Rolled back because another change failed",
  notRun: "Not run because an earlier change failed",
};

export function statusText(status: EditStatus, message?: string): string {
  if (status === "failed") return message ?? STATUS_TEXT.failed;
  if (status === "conflict" && message) return `${STATUS_TEXT.conflict}: ${message}`;
  return STATUS_TEXT[status];
}

/** The problem shown on a grid row, if the last apply reported one for it. */
export function problemOf(
  problems: readonly EditProblem[],
  changes: PendingChanges,
  row: GridRow,
): EditProblem | undefined {
  if (row.type === "new") {
    const edit = changes.edits[row.edit];
    return problems.find((p) => p.edit === edit);
  }
  return problems.find((p) => p.edit.op !== "insert" && p.edit.row === row.row);
}

export interface FoldedApply {
  applied: AppliedState;
  changes: PendingChanges;
  problems: EditProblem[];
}

/**
 * Folds an `edit_apply` report: a commit moves the buffer into the applied state; a rollback
 * keeps the buffer and reports each edit's status (failed and conflicting edits first).
 */
export function foldReport(
  changes: PendingChanges,
  report: ApplyReport,
  applied: AppliedState,
  stored: number,
): FoldedApply {
  if (report.committed) {
    const values = new Map(applied.values);
    const deleted = new Set(applied.deleted);
    let extra = applied.extraRows;
    report.outcomes.forEach((outcome, i) => {
      const edit = changes.edits[i];
      if (!edit) return;
      const row = outcome.row ?? (edit.op === "insert" ? undefined : edit.row);
      if (row === undefined) return;
      if (edit.op === "delete") {
        deleted.add(row);
        values.delete(row);
        return;
      }
      if (outcome.values) values.set(row, outcome.values);
      if (row >= stored) extra = Math.max(extra, row - stored + 1);
    });
    return { applied: { values, deleted, extraRows: extra }, changes: emptyChanges, problems: [] };
  }
  const problems: EditProblem[] = [];
  report.outcomes.forEach((outcome, index) => {
    const edit = changes.edits[index];
    if (!edit || outcome.status === "applied") return;
    problems.push({
      edit,
      index,
      status: outcome.status,
      message: statusText(outcome.status, outcome.error?.message),
    });
  });
  const rank = (s: EditStatus) => (s === "failed" || s === "conflict" ? 0 : 1);
  problems.sort((a, b) => rank(a.status) - rank(b.status) || a.index - b.index);
  return { applied, changes, problems };
}

/** How an edit is named in messages: "Row 4", "New row 2", "Delete row 7". */
export function editLabel(changes: PendingChanges, edit: PendingEdit): string {
  if (edit.op === "insert") {
    const ordinal = insertIndexes(changes).indexOf(changes.edits.indexOf(edit)) + 1;
    return ordinal > 0 ? `New row ${ordinal}` : "New row";
  }
  return edit.op === "delete" ? `Delete row ${edit.row + 1}` : `Row ${edit.row + 1}`;
}

/** A short summary of the buffer for the toolbar: "2 updated, 1 new, 1 deleted". */
export function changesSummary(changes: PendingChanges): string {
  let updated = 0;
  let inserted = 0;
  let deleted = 0;
  for (const e of changes.edits) {
    if (e.op === "update") updated += 1;
    else if (e.op === "insert") inserted += 1;
    else deleted += 1;
  }
  const parts: string[] = [];
  if (updated) parts.push(`${updated} updated`);
  if (inserted) parts.push(`${inserted} new`);
  if (deleted) parts.push(`${deleted} deleted`);
  return parts.join(", ");
}

/**
 * The cells of a copy of a row (for "Duplicate row"): every editable column with its value,
 * except key columns that have a default (a generated key is generated again).
 */
export function duplicateCells(
  columns: readonly EditColumn[],
  value: (column: EditColumn) => string | null | undefined,
): CellEdit[] {
  const cells: CellEdit[] = [];
  for (const column of columns) {
    if (!column.editable || (column.key && column.hasDefault)) continue;
    const v = value(column);
    if (v === undefined) continue;
    cells.push({
      column: column.name,
      input: v === null ? { type: "null" } : { type: "text", text: v },
    });
  }
  return cells;
}

/** Whether a cell of `row` may be edited. */
export function cellEditable(
  column: EditColumn | undefined,
  row: GridRow | null,
  changes: PendingChanges,
  applied: AppliedState,
): boolean {
  if (!column?.editable || !row) return false;
  const status = rowStatus(changes, applied, row);
  return status !== "deleted" && status !== "removed";
}
