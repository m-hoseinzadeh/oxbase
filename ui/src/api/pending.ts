/**
 * Pure helpers for the grid's pending-changes buffer, mirroring `PendingChanges` in
 * `oxbase_services::edit` (same merge rules): at most one update or delete per row; a cell
 * edit replaces that column's earlier input; deleting a row drops its update; a row marked for
 * deletion cannot be edited. Every helper returns a new buffer (React state friendly).
 */
import type { CellEdit, CellInput, PendingChanges, PendingEdit } from "./types";

export const emptyChanges: PendingChanges = { edits: [] };

function rowIndex(changes: PendingChanges, row: number): number {
  return changes.edits.findIndex((e) => e.op !== "insert" && e.row === row);
}

/** Sets one cell of an existing row. Throws when the row is marked for deletion. */
export function setCell(
  changes: PendingChanges,
  row: number,
  column: string,
  input: CellInput,
): PendingChanges {
  const at = rowIndex(changes, row);
  const cell: CellEdit = { column, input };
  if (at < 0) {
    return { edits: [...changes.edits, { op: "update", row, cells: [cell] }] };
  }
  const current = changes.edits[at] as PendingEdit;
  if (current.op === "delete") {
    throw new Error(`row ${row} is marked for deletion`);
  }
  if (current.op !== "update") return changes;
  const cells = current.cells.some((c) => c.column === column)
    ? current.cells.map((c) => (c.column === column ? cell : c))
    : [...current.cells, cell];
  const edits = changes.edits.slice();
  edits[at] = { op: "update", row, cells };
  return { edits };
}

/** Adds a new row; returns the buffer and the new edit's index. */
export function insertRow(
  changes: PendingChanges,
  cells: CellEdit[] = [],
): { changes: PendingChanges; index: number } {
  return {
    changes: { edits: [...changes.edits, { op: "insert", cells }] },
    index: changes.edits.length,
  };
}

/** Sets a cell of a new row (by its edit index). */
export function setNewCell(
  changes: PendingChanges,
  index: number,
  column: string,
  input: CellInput,
): PendingChanges {
  const current = changes.edits[index];
  if (current?.op !== "insert") return changes;
  const cell: CellEdit = { column, input };
  const cells = current.cells.some((c) => c.column === column)
    ? current.cells.map((c) => (c.column === column ? cell : c))
    : [...current.cells, cell];
  const edits = changes.edits.slice();
  edits[index] = { op: "insert", cells };
  return { edits };
}

/** Marks an existing row for deletion, dropping its pending update. */
export function deleteRow(changes: PendingChanges, row: number): PendingChanges {
  const at = rowIndex(changes, row);
  const edits = changes.edits.slice();
  if (at < 0) edits.push({ op: "delete", row });
  else edits[at] = { op: "delete", row };
  return { edits };
}

/** Removes a new row (by its edit index). */
export function removeNewRow(changes: PendingChanges, index: number): PendingChanges {
  if (changes.edits[index]?.op !== "insert") return changes;
  return { edits: changes.edits.filter((_, i) => i !== index) };
}

/** Forgets every pending edit of an existing row. */
export function revertRow(changes: PendingChanges, row: number): PendingChanges {
  const at = rowIndex(changes, row);
  if (at < 0) return changes;
  return { edits: changes.edits.filter((_, i) => i !== at) };
}
