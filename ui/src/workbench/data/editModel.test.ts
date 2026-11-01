import { describe, expect, it } from "vitest";
import {
  deleteRow,
  type EditColumn,
  emptyChanges,
  insertRow,
  type PendingChanges,
  setCell,
  setNewCell,
} from "../../api";
import {
  appliedCount,
  changesSummary,
  duplicateCells,
  editLabel,
  foldReport,
  gridIndexOfInsert,
  gridRow,
  gridRowCount,
  NOTHING_APPLIED,
  pendingInput,
  problemOf,
  rowStatus,
} from "./editModel";

function column(name: string, index: number, patch: Partial<EditColumn> = {}): EditColumn {
  return {
    name,
    index,
    dataType: { type: "text" },
    nativeType: "text",
    nullable: true,
    hasDefault: false,
    key: false,
    editable: true,
    input: { type: "text" },
    ...patch,
  };
}

function buffer(): PendingChanges {
  let c = setCell(emptyChanges, 0, "name", { type: "text", text: "bob" });
  c = insertRow(c).changes;
  c = setNewCell(c, 1, "name", { type: "null" });
  c = deleteRow(c, 4);
  return insertRow(c).changes;
}

describe("edit model", () => {
  it("lists stored rows, applied inserts, then pending new rows", () => {
    const changes = buffer();
    const applied = { ...NOTHING_APPLIED, extraRows: 1 };
    expect(gridRowCount(5, applied, changes)).toBe(8);
    expect(gridRow(4, 5, applied, changes)).toEqual({ type: "row", row: 4 });
    expect(gridRow(5, 5, applied, changes)).toEqual({ type: "row", row: 5 });
    expect(gridRow(6, 5, applied, changes)).toEqual({ type: "new", edit: 1, ordinal: 1 });
    expect(gridRow(7, 5, applied, changes)).toEqual({ type: "new", edit: 3, ordinal: 2 });
    expect(gridRow(8, 5, applied, changes)).toBeNull();
    expect(gridIndexOfInsert(3, 5, applied, changes)).toBe(7);
  });

  it("reads pending cells and row states", () => {
    const changes = buffer();
    expect(pendingInput(changes, { type: "row", row: 0 }, "name")).toEqual({
      type: "text",
      text: "bob",
    });
    expect(pendingInput(changes, { type: "new", edit: 1, ordinal: 1 }, "name")).toEqual({
      type: "null",
    });
    expect(pendingInput(changes, { type: "row", row: 4 }, "name")).toBeUndefined();
    const applied = { ...NOTHING_APPLIED, deleted: new Set([2]) };
    expect(rowStatus(changes, applied, { type: "row", row: 0 })).toBe("updated");
    expect(rowStatus(changes, applied, { type: "row", row: 4 })).toBe("deleted");
    expect(rowStatus(changes, applied, { type: "row", row: 2 })).toBe("removed");
    expect(rowStatus(changes, applied, { type: "row", row: 3 })).toBeNull();
    expect(changesSummary(changes)).toBe("1 updated, 2 new, 1 deleted");
  });

  it("folds a committed apply into the applied state", () => {
    const changes = buffer();
    const folded = foldReport(
      changes,
      {
        committed: true,
        outcomes: [
          { status: "applied", row: 0, values: ["1", "bob"] },
          { status: "applied", row: 5, values: ["9", null] },
          { status: "applied", row: 4 },
          { status: "applied", row: 6, values: ["10", null] },
        ],
      },
      NOTHING_APPLIED,
      5,
    );
    expect(folded.changes).toEqual(emptyChanges);
    expect(folded.problems).toEqual([]);
    expect(folded.applied.extraRows).toBe(2);
    expect(folded.applied.values.get(0)).toEqual(["1", "bob"]);
    expect(folded.applied.values.get(6)).toEqual(["10", null]);
    expect(folded.applied.deleted.has(4)).toBe(true);
  });

  it("keeps the buffer of a rolled-back apply and reports each edit", () => {
    const changes = buffer();
    const folded = foldReport(
      changes,
      {
        committed: false,
        outcomes: [
          { status: "rolledBack", row: 0 },
          { status: "failed", error: { category: "sql", message: "null value in name" } },
          { status: "notRun", row: 4 },
          { status: "notRun" },
        ],
      },
      NOTHING_APPLIED,
      5,
    );
    expect(folded.changes).toBe(changes);
    expect(folded.problems.map((p) => [p.index, p.status])).toEqual([
      [1, "failed"],
      [0, "rolledBack"],
      [2, "notRun"],
      [3, "notRun"],
    ]);
    expect(folded.problems[0]?.message).toBe("null value in name");
    const newRow = { type: "new" as const, edit: 1, ordinal: 1 };
    expect(problemOf(folded.problems, changes, newRow)?.status).toBe("failed");
    expect(problemOf(folded.problems, changes, { type: "row", row: 4 })?.status).toBe("notRun");
    expect(editLabel(changes, changes.edits[1] as never)).toBe("New row 1");
    expect(editLabel(changes, changes.edits[2] as never)).toBe("Delete row 5");
  });

  it("folds the kept edits of a stopped apply on a table that cannot roll back", () => {
    const changes = buffer();
    const report = {
      committed: false,
      outcomes: [
        { status: "applied" as const, row: 0, values: ["1", "bob"], warning: "kept" },
        { status: "applied" as const, row: 5, values: ["9", null], warning: "kept" },
        { status: "conflict" as const, row: 4 },
        { status: "notRun" as const },
      ],
    };
    const folded = foldReport(changes, report, NOTHING_APPLIED, 5);
    expect(appliedCount(report)).toBe(2);
    expect(folded.applied.values.get(0)).toEqual(["1", "bob"]);
    expect(folded.applied.values.get(5)).toEqual(["9", null]);
    expect(folded.applied.extraRows).toBe(1);
    // The kept edits left the buffer; the conflicting and unrun ones stay.
    expect(folded.changes.edits).toEqual([changes.edits[2], changes.edits[3]]);
    expect(folded.problems.map((p) => [p.index, p.status])).toEqual([
      [2, "conflict"],
      [3, "notRun"],
    ]);
    const newRow = { type: "new" as const, edit: 1, ordinal: 1 };
    expect(problemOf(folded.problems, folded.changes, newRow)?.status).toBe("notRun");
    expect(rowStatus(folded.changes, folded.applied, { type: "row", row: 4 })).toBe("deleted");
  });

  it("reports conflicts with their meaning", () => {
    const changes = setCell(emptyChanges, 2, "name", { type: "text", text: "x" });
    const folded = foldReport(
      changes,
      { committed: false, outcomes: [{ status: "conflict", row: 2 }] },
      NOTHING_APPLIED,
      5,
    );
    expect(folded.problems[0]?.message).toMatch(/Conflict: the row was changed or deleted/);
  });

  it("duplicates editable values except generated keys", () => {
    const columns = [
      column("id", 0, { key: true, hasDefault: true }),
      column("code", 1, { key: false }),
      column("total", 2, { editable: false }),
      column("note", 3),
    ];
    const values: Record<string, string | null> = { id: "1", code: "A", total: "9", note: null };
    expect(duplicateCells(columns, (c) => values[c.name])).toEqual([
      { column: "code", input: { type: "text", text: "A" } },
      { column: "note", input: { type: "null" } },
    ]);
  });
});
