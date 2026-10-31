import { beforeEach, describe, expect, it, vi } from "vitest";

const core = vi.hoisted(() => {
  class Channel<T> {
    onmessage: (message: T) => void = () => {};
  }
  return {
    Channel,
    invoke: vi.fn<(cmd: string, args?: unknown) => Promise<unknown>>(),
    isTauri: vi.fn<() => boolean>(() => true),
  };
});
vi.mock("@tauri-apps/api/core", () => core);

import { editApply, editBegin, rowGetForm, tableDataOpen } from "./data";
import { capabilitiesGet, designerApply, designerLoad, designerPreview } from "./designer";
import {
  deleteRow,
  emptyChanges,
  insertRow,
  removeNewRow,
  revertRow,
  setCell,
  setNewCell,
} from "./pending";
import {
  filterOpArity,
  newTableDesign,
  type ObjectRef,
  type RunEvent,
  type TableDataQuery,
} from "./types";

const table: ObjectRef = { kind: "table", catalog: "db", schema: "public", name: "t" };

describe("table data and editing wrappers", () => {
  beforeEach(() => {
    core.invoke.mockReset();
    core.isTauri.mockReturnValue(true);
  });

  it("opens table data with a channel and explicit nulls", async () => {
    core.invoke.mockResolvedValue("run-1");
    const request: TableDataQuery = {
      table,
      filter: {
        type: "and",
        items: [
          { type: "condition", column: "id", op: "gt", values: [{ type: "text", value: "3" }] },
          { type: "rawSql", sql: "note IS NOT NULL" },
        ],
      },
      sort: [{ column: "id", descending: true, nulls: "last" }],
    };
    const seen: RunEvent[] = [];
    expect(await tableDataOpen("s1", "tab-1", request, (e) => seen.push(e))).toBe("run-1");
    const [cmd, args] = core.invoke.mock.calls[0] ?? [];
    expect(cmd).toBe("table_data_open");
    const { onEvent, ...rest } = args as { onEvent: { onmessage: (e: RunEvent) => void } };
    expect(rest).toEqual({
      sessionId: "s1",
      editorId: "tab-1",
      request,
      options: null,
      requestId: null,
    });
    onEvent.onmessage({ type: "finished", elapsedMs: 1 });
    expect(seen).toHaveLength(1);
  });

  it("forwards edit commands", async () => {
    core.invoke.mockResolvedValue({});
    await editBegin("run-1");
    expect(core.invoke).toHaveBeenLastCalledWith("edit_begin", { runId: "run-1", requestId: null });
    const changes = setCell(emptyChanges, 0, "name", { type: "text", text: "x" });
    await editApply("run-1", changes, "r1");
    expect(core.invoke).toHaveBeenLastCalledWith("edit_apply", {
      runId: "run-1",
      changes: {
        edits: [
          { op: "update", row: 0, cells: [{ column: "name", input: { type: "text", text: "x" } }] },
        ],
      },
      requestId: "r1",
    });
    await rowGetForm("run-1", 0, 12);
    expect(core.invoke).toHaveBeenLastCalledWith("row_get_form", {
      runId: "run-1",
      resultIndex: 0,
      row: 12,
    });
  });

  it("forwards designer commands", async () => {
    core.invoke.mockResolvedValue({});
    await designerLoad("s1", table);
    expect(core.invoke).toHaveBeenLastCalledWith("designer_load", {
      sessionId: "s1",
      table,
      requestId: null,
    });
    const design = newTableDesign(table);
    expect(design).toEqual({
      table,
      columns: [],
      indexes: [],
      foreignKeys: [],
      checks: [],
      extras: {},
    });
    await designerPreview("s1", design);
    expect(core.invoke).toHaveBeenLastCalledWith("designer_preview", {
      sessionId: "s1",
      design,
      requestId: null,
    });
    await designerApply("s1", design, { confirmDestructive: true });
    expect(core.invoke).toHaveBeenLastCalledWith("designer_apply", {
      sessionId: "s1",
      design,
      options: { confirmDestructive: true },
      requestId: null,
    });
    await capabilitiesGet("s1");
    expect(core.invoke).toHaveBeenLastCalledWith("capabilities_get", { sessionId: "s1" });
  });

  it("normalises failures to command errors", async () => {
    core.invoke.mockRejectedValue({ category: "invalid_input", message: "row 3 is deleted" });
    await expect(editApply("run-1", emptyChanges)).rejects.toEqual({
      category: "invalid_input",
      message: "row 3 is deleted",
    });
  });
});

describe("pending changes buffer", () => {
  it("merges edits per row like the Rust buffer", () => {
    let changes = setCell(emptyChanges, 3, "name", { type: "text", text: "a" });
    changes = setCell(changes, 3, "name", { type: "text", text: "b" });
    changes = setCell(changes, 3, "note", { type: "null" });
    const added = insertRow(changes);
    expect(added.index).toBe(1);
    changes = setNewCell(added.changes, added.index, "name", { type: "default" });
    changes = deleteRow(changes, 7);
    expect(changes.edits).toEqual([
      {
        op: "update",
        row: 3,
        cells: [
          { column: "name", input: { type: "text", text: "b" } },
          { column: "note", input: { type: "null" } },
        ],
      },
      { op: "insert", cells: [{ column: "name", input: { type: "default" } }] },
      { op: "delete", row: 7 },
    ]);
    changes = deleteRow(changes, 3);
    expect(changes.edits[0]).toEqual({ op: "delete", row: 3 });
    expect(() => setCell(changes, 3, "name", { type: "null" })).toThrow(/deletion/);
    changes = revertRow(revertRow(changes, 3), 7);
    expect(changes.edits).toHaveLength(1);
    expect(removeNewRow(changes, 0).edits).toEqual([]);
    expect(emptyChanges.edits).toEqual([]);
  });

  it("knows operator arity", () => {
    expect(filterOpArity("isNull")).toBe("none");
    expect(filterOpArity("between")).toBe("two");
    expect(filterOpArity("in")).toBe("many");
    expect(filterOpArity("contains")).toBe("one");
  });
});
