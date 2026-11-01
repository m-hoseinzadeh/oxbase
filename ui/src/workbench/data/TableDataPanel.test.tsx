import { act, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import type {
  ApplyReport,
  Editability,
  EngineCapabilities,
  ObjectRef,
  PendingChanges,
  RunEvent,
  StatementSpan,
} from "../../api";
import { tauriCore } from "../../test/tauriMock";
import { NO_DESIGNER } from "../designer/designModel";
import type { ResultGridProps } from "../results/ResultGrid";
import { TableDataPanel } from "./TableDataPanel";

vi.mock("@tauri-apps/api/core", async () => (await import("../../test/tauriMock")).tauriCore);

// The canvas grid cannot draw in jsdom: a stand-in shows what the editable grid would draw
// (values, highlights) and opens the same overlay editor the real grid opens.
vi.mock("../results/ResultGrid", async () => {
  const { useState } = await import("react");
  return {
    ResultGrid: ({ rowCount, columns, edit, onSelectRows, selectedRows }: ResultGridProps) => {
      const [open, setOpen] = useState<[number, number] | null>(null);
      const rows = rowCount + (edit?.extraRows ?? 0);
      const isSelected = (r: number) =>
        selectedRows?.current === r || !!selectedRows?.ranges.some(([a, b]) => r >= a && r < b);
      return (
        <div data-testid="grid">
          <button
            type="button"
            onClick={() =>
              onSelectRows?.({
                current: null,
                ranges: [
                  [0, 1],
                  [2, 3],
                ],
              })
            }
          >
            select rows 0 and 2
          </button>
          {Array.from({ length: rows }, (_, r) => (
            // biome-ignore lint/suspicious/noArrayIndexKey: rows are positions.
            <div key={r} data-testid={`row-${r}`} data-selected={isSelected(r) ? "yes" : "no"}>
              <button
                type="button"
                onClick={() => onSelectRows?.({ current: r, ranges: [[r, r + 1]] })}
              >
                select {r}
              </button>
              {columns.map((c, col) => {
                const o = edit?.override(r, col);
                return (
                  <span
                    key={c.name}
                    data-testid={`cell-${r}-${c.name}`}
                    data-highlight={edit?.highlight(r, col) ?? ""}
                  >
                    {o ? (o.placeholder ?? o.text ?? "NULL") : "stored"}
                  </span>
                );
              })}
              {columns.map((c, col) =>
                edit?.canEdit(r, col) ? (
                  <button key={c.name} type="button" onClick={() => setOpen([r, col])}>
                    edit {r} {c.name}
                  </button>
                ) : null,
              )}
            </div>
          ))}
          {open && edit ? (
            <div data-testid="overlay">
              {edit.renderEditor({ row: open[0], col: open[1], finish: () => setOpen(null) })}
              <button type="button" onClick={() => setOpen(null)}>
                click outside
              </button>
            </div>
          ) : null}
        </div>
      );
    },
  };
});

const TABLE: ObjectRef = { kind: "table", catalog: "app", schema: "public", name: "people" };

const SPAN: StatementSpan = {
  index: 0,
  startByte: 0,
  endByte: 10,
  startLine: 1,
  startColumn: 1,
  endLine: 1,
  endColumn: 11,
  returnsRows: true,
};

const CAPS: EngineCapabilities = {
  engine: "postgres",
  serverVersion: { major: 18, minor: 0, patch: 0, build: 0, raw: "18.0" },
  designer: NO_DESIGNER,
  gridEditing: true,
  nullsOrdering: true,
};

const EDITABLE: Editability = {
  runId: "run-1",
  resultIndex: 0,
  table: TABLE,
  editable: true,
  transactional: true,
  key: { table: TABLE, kind: "primary", columns: ["id"] },
  columns: [
    {
      name: "id",
      index: 0,
      dataType: { type: "int", bits: 64, unsigned: false },
      nativeType: "bigint",
      nullable: false,
      hasDefault: true,
      key: true,
      editable: true,
      input: { type: "integer" },
    },
    {
      name: "name",
      index: 1,
      dataType: { type: "text" },
      nativeType: "text",
      nullable: false,
      hasDefault: false,
      key: false,
      editable: true,
      input: { type: "text" },
    },
    {
      name: "note",
      index: 2,
      dataType: { type: "text" },
      nativeType: "text",
      nullable: true,
      hasDefault: false,
      key: false,
      editable: true,
      input: { type: "text" },
    },
  ],
};

let editability: Editability;
let applyReport: (changes: PendingChanges) => ApplyReport;
let opens: Array<Record<string, unknown>>;

function runEvents(runId: string): RunEvent[] {
  return [
    { type: "started", runId, statements: [SPAN] },
    {
      type: "resultSet",
      resultIndex: 0,
      statementIndex: 0,
      statementRange: SPAN,
      columns: [
        { name: "id", dataType: "Int64", nullable: false },
        { name: "name", dataType: "Utf8", nullable: false },
        { name: "note", dataType: "Utf8", nullable: true },
      ],
    },
    { type: "resultDone", resultIndex: 0, totalRows: 3, truncated: false },
    { type: "statementDone", statementIndex: 0, elapsedMs: 2 },
    { type: "finished", elapsedMs: 3 },
  ];
}

function calls(cmd: string): Array<Record<string, unknown>> {
  return tauriCore.invoke.mock.calls
    .filter(([c]) => c === cmd)
    .map(([, a]) => (a ?? {}) as Record<string, unknown>);
}

beforeEach(() => {
  tauriCore.invoke.mockReset();
  tauriCore.isTauri.mockReturnValue(true);
  editability = EDITABLE;
  applyReport = (changes) => ({
    committed: true,
    outcomes: changes.edits.map(() => ({ status: "applied" as const })),
  });
  opens = [];
  tauriCore.invoke.mockImplementation(async (cmd, args) => {
    const a = (args ?? {}) as Record<string, unknown>;
    switch (cmd) {
      case "capabilities_get":
        return CAPS;
      case "table_data_open": {
        opens.push(a);
        const runId = `run-${opens.length}`;
        const channel = a.onEvent as { onmessage: (e: RunEvent) => void };
        setTimeout(() => {
          for (const event of runEvents(runId)) channel.onmessage(event);
        }, 0);
        return runId;
      }
      case "edit_begin":
        return { ...editability, runId: a.runId };
      case "edit_apply":
        return applyReport(a.changes as PendingChanges);
      case "row_get_form": {
        const row = a.row as number;
        return {
          row,
          fields: [
            { column: "id", text: String(row + 1) },
            { column: "name", text: `name${row}` },
            ...(row === 0 ? [{ column: "note" }] : [{ column: "note", text: `note${row}` }]),
          ],
          json: JSON.stringify({
            id: row + 1,
            name: `name${row}`,
            note: row === 0 ? null : `note${row}`,
          }),
        };
      }
      case "query_close_editor":
      case "cancel_request":
        return null;
      default:
        throw new Error(`unexpected ${cmd}`);
    }
  });
});

async function renderPanel() {
  render(<TableDataPanel editorId="editor-t1" sessionId="s-1" table={TABLE} />);
  await screen.findByTestId("grid");
}

/** Opens the overlay editor of a cell and waits until its value is loaded. */
async function openCell(row: number, column: string) {
  fireEvent.click(await screen.findByRole("button", { name: `edit ${row} ${column}` }));
  const overlay = screen.getByTestId("overlay");
  await waitFor(() => expect(within(overlay).queryByText("Loading…")).not.toBeInTheDocument());
  return overlay;
}

describe("TableDataPanel", () => {
  it("opens the table's rows in its editor and closes the editor with the tab", async () => {
    const { unmount } = render(
      <TableDataPanel editorId="editor-t1" sessionId="s-1" table={TABLE} />,
    );
    await screen.findByTestId("grid");
    expect(opens[0]).toMatchObject({
      sessionId: "s-1",
      editorId: "editor-t1",
      request: { table: TABLE, sort: [] },
    });
    expect(opens[0]?.request).not.toHaveProperty("filter");
    await waitFor(() => expect(calls("edit_begin")).toEqual([{ runId: "run-1", requestId: null }]));
    expect(screen.getByTestId("table-data-status")).toHaveTextContent("3 rows");
    unmount();
    expect(calls("query_close_editor")).toEqual([{ editorId: "editor-t1" }]);
  });

  it("shows why a result is read-only and offers no editing", async () => {
    editability = {
      ...EDITABLE,
      editable: false,
      readOnlyReason: "the table has no primary key or usable unique key",
    };
    await renderPanel();
    const banner = await screen.findByTestId("read-only-banner");
    expect(banner).toHaveTextContent(
      "Read-only: the table has no primary key or usable unique key",
    );
    expect(screen.getByRole("button", { name: "Add row" })).toBeDisabled();
    expect(screen.queryByRole("button", { name: /^edit / })).not.toBeInTheDocument();
  });

  it("collects cell edits, new rows and deletes in the pending buffer and applies them", async () => {
    applyReport = () => ({
      committed: true,
      outcomes: [
        { status: "applied", row: 0, values: ["1", "bob", null] },
        { status: "applied", row: 1, values: ["2", "name1", null] },
        { status: "applied", row: 3, values: ["4", "carol", null] },
        { status: "applied", row: 2 },
      ],
    });
    await renderPanel();

    // Typed edit of a stored cell: starts from the exact stored text, Enter keeps it.
    let overlay = await openCell(0, "name");
    const input = within(overlay).getByRole("textbox", { name: "name" });
    expect(input).toHaveValue("name0");
    fireEvent.change(input, { target: { value: "bob" } });
    fireEvent.keyDown(input, { key: "Enter" });
    expect(screen.queryByTestId("overlay")).not.toBeInTheDocument();
    expect(screen.getByTestId("cell-0-name")).toHaveTextContent("bob");
    expect(screen.getByTestId("cell-0-name")).toHaveAttribute("data-highlight", "changed");

    // NULL is its own state (not an empty string); leaving the overlay keeps it.
    overlay = await openCell(1, "note");
    fireEvent.click(within(overlay).getByRole("checkbox", { name: "note is NULL" }));
    fireEvent.click(within(overlay).getByRole("button", { name: "click outside" }));
    expect(screen.getByTestId("cell-1-note")).toHaveTextContent("NULL");

    // Escape drops an edit.
    overlay = await openCell(2, "name");
    const escaped = within(overlay).getByRole("textbox", { name: "name" });
    fireEvent.change(escaped, { target: { value: "dropped" } });
    fireEvent.keyDown(escaped, { key: "Escape" });
    fireEvent.click(within(overlay).getByRole("button", { name: "click outside" }));
    expect(screen.getByTestId("cell-2-name")).toHaveTextContent("stored");

    // A new row starts with defaults.
    fireEvent.click(screen.getByRole("button", { name: "Add row" }));
    expect(screen.getByTestId("cell-3-id")).toHaveTextContent("DEFAULT");
    expect(screen.getByTestId("cell-3-id")).toHaveAttribute("data-highlight", "inserted");
    overlay = await openCell(3, "name");
    fireEvent.click(within(overlay).getByRole("button", { name: /name: DEFAULT/ }));
    const fresh = within(overlay).getByRole("textbox", { name: "name" });
    fireEvent.change(fresh, { target: { value: "carol" } });
    fireEvent.keyDown(fresh, { key: "Enter" });

    // Delete the selected row.
    fireEvent.click(screen.getByRole("button", { name: "select 2" }));
    fireEvent.click(screen.getByRole("button", { name: "Delete row" }));
    expect(screen.getByTestId("cell-2-name")).toHaveAttribute("data-highlight", "deleted");
    expect(screen.queryByRole("button", { name: "edit 2 name" })).not.toBeInTheDocument();
    expect(screen.getByTestId("table-data-status")).toHaveTextContent(
      "2 updated, 1 new, 1 deleted pending",
    );

    fireEvent.click(screen.getByRole("button", { name: "Apply (4)" }));
    await screen.findByText("Saved 4 changes.");
    expect(calls("edit_apply")).toEqual([
      {
        runId: "run-1",
        requestId: null,
        changes: {
          edits: [
            {
              op: "update",
              row: 0,
              cells: [{ column: "name", input: { type: "text", text: "bob" } }],
            },
            { op: "update", row: 1, cells: [{ column: "note", input: { type: "null" } }] },
            { op: "insert", cells: [{ column: "name", input: { type: "text", text: "carol" } }] },
            { op: "delete", row: 2 },
          ],
        },
      },
    ]);
    // Applied values replace the stored ones; the inserted row is numbered after them.
    expect(screen.getByTestId("cell-0-name")).toHaveTextContent("bob");
    expect(screen.getByTestId("cell-0-name")).toHaveAttribute("data-highlight", "");
    expect(screen.getByTestId("cell-3-id")).toHaveTextContent("4");
    expect(screen.getByTestId("cell-2-name")).toHaveAttribute("data-highlight", "deleted");
    expect(screen.getByRole("button", { name: "Apply" })).toBeDisabled();
  });

  it("deletes exactly the selected rows when the selection has a gap", async () => {
    await renderPanel();
    await waitFor(() => expect(screen.getByRole("button", { name: "Add row" })).toBeEnabled());
    fireEvent.click(screen.getByRole("button", { name: "select rows 0 and 2" }));
    fireEvent.click(screen.getByRole("button", { name: "Delete 2 rows" }));
    expect(screen.getByTestId("cell-0-name")).toHaveAttribute("data-highlight", "deleted");
    expect(screen.getByTestId("cell-1-name")).toHaveAttribute("data-highlight", "");
    expect(screen.getByTestId("cell-2-name")).toHaveAttribute("data-highlight", "deleted");
    fireEvent.click(screen.getByRole("button", { name: "Apply (2)" }));
    await screen.findByText("Saved 2 changes.");
    expect(calls("edit_apply")[0]?.changes).toEqual({
      edits: [
        { op: "delete", row: 0 },
        { op: "delete", row: 2 },
      ],
    });
  });

  it("selects a new row in the grid, so Delete acts on the highlighted row", async () => {
    await renderPanel();
    await waitFor(() => expect(screen.getByRole("button", { name: "Add row" })).toBeEnabled());
    fireEvent.click(screen.getByRole("button", { name: "select 1" }));
    expect(screen.getByTestId("row-1")).toHaveAttribute("data-selected", "yes");
    fireEvent.click(screen.getByRole("button", { name: "Add row" }));
    expect(screen.getByTestId("row-1")).toHaveAttribute("data-selected", "no");
    expect(screen.getByTestId("row-3")).toHaveAttribute("data-selected", "yes");
    fireEvent.click(screen.getByRole("button", { name: "Delete row" }));
    expect(screen.queryByTestId("row-3")).not.toBeInTheDocument();
    expect(screen.getByTestId("cell-1-name")).toHaveAttribute("data-highlight", "");
  });

  it("keeps the buffer and shows each edit's error when an apply rolls back", async () => {
    applyReport = () => ({
      committed: false,
      outcomes: [{ status: "conflict", row: 0 }, { status: "notRun" }],
    });
    await renderPanel();
    const overlay = await openCell(0, "name");
    const input = within(overlay).getByRole("textbox", { name: "name" });
    fireEvent.change(input, { target: { value: "x" } });
    fireEvent.keyDown(input, { key: "Enter" });
    fireEvent.click(screen.getByRole("button", { name: "Add row" }));

    fireEvent.click(screen.getByRole("button", { name: "Apply (2)" }));
    const errors = await screen.findByTestId("apply-errors");
    expect(errors).toHaveTextContent("Nothing was saved");
    expect(errors).toHaveTextContent(
      "Row 1: Conflict: the row was changed or deleted since it was loaded",
    );
    expect(errors).toHaveTextContent("New row 1: Not run because an earlier change failed");
    expect(screen.getByTestId("cell-0-name")).toHaveAttribute("data-highlight", "error");
    expect(screen.getByTestId("cell-0-name")).toHaveTextContent("x");
    expect(screen.getByRole("button", { name: "Apply (2)" })).toBeEnabled();

    // Discard drops the buffer and the errors.
    fireEvent.click(screen.getByRole("button", { name: "Discard" }));
    expect(screen.queryByTestId("apply-errors")).not.toBeInTheDocument();
    expect(screen.getByTestId("cell-0-name")).toHaveTextContent("stored");
  });

  it("warns on a table that cannot roll back and keeps what an apply saved", async () => {
    editability = {
      ...EDITABLE,
      transactional: false,
      warning: "this table's storage engine does not support transactions",
    };
    applyReport = () => ({
      committed: false,
      outcomes: [
        { status: "applied", row: 0, values: ["1", "x", null], warning: "kept" },
        { status: "conflict", row: 1 },
      ],
    });
    await renderPanel();
    expect(await screen.findByTestId("edit-warning-banner")).toHaveTextContent(
      "Note: this table's storage engine does not support transactions",
    );
    for (const row of [0, 1]) {
      const overlay = await openCell(row, "name");
      const input = within(overlay).getByRole("textbox", { name: "name" });
      fireEvent.change(input, { target: { value: row === 0 ? "x" : "y" } });
      fireEvent.keyDown(input, { key: "Enter" });
    }

    fireEvent.click(screen.getByRole("button", { name: "Apply (2)" }));
    const errors = await screen.findByTestId("apply-errors");
    expect(errors).toHaveTextContent("Saved 1 of 2 changes; the rest were not saved");
    expect(errors).not.toHaveTextContent("rolled back");
    expect(errors).toHaveTextContent("Row 2: Conflict");
    // The saved edit left the buffer; the conflicting one stays.
    expect(screen.getByRole("button", { name: "Apply (1)" })).toBeEnabled();
    expect(screen.getByTestId("cell-0-name")).toHaveTextContent("x");
    expect(screen.getByTestId("cell-1-name")).toHaveAttribute("data-highlight", "error");
  });

  it("shows an apply that failed as a whole", async () => {
    await renderPanel();
    const overlay = await openCell(0, "name");
    const input = within(overlay).getByRole("textbox", { name: "name" });
    fireEvent.change(input, { target: { value: "x" } });
    fireEvent.keyDown(input, { key: "Enter" });
    applyReport = () => {
      throw { category: "invalid_input", message: "the run is still running" };
    };
    fireEvent.click(screen.getByRole("button", { name: "Apply (1)" }));
    expect(await screen.findByTestId("apply-errors")).toHaveTextContent("the run is still running");
  });

  it("edits the current row in the form view and shows it as JSON", async () => {
    await renderPanel();
    await screen.findByRole("button", { name: "edit 1 name" });
    fireEvent.click(screen.getByRole("button", { name: "select 1" }));
    fireEvent.click(screen.getByRole("tab", { name: "Form" }));
    const form = await screen.findByTestId("form-view");
    expect(form).toHaveTextContent("Row 2 of 3");
    const name = await within(form).findByRole("textbox", { name: "name" });
    expect(name).toHaveValue("name1");
    fireEvent.change(name, { target: { value: "renamed" } });
    expect(screen.getByRole("button", { name: "Apply (1)" })).toBeEnabled();
    // Non-nullable columns have no NULL toggle; nullable ones do.
    expect(within(form).queryByRole("checkbox", { name: "name is NULL" })).not.toBeInTheDocument();
    fireEvent.click(within(form).getByRole("checkbox", { name: "note is NULL" }));

    fireEvent.click(within(form).getByRole("button", { name: "Previous row" }));
    expect(
      await within(screen.getByTestId("form-view")).findByText("Row 1 of 3"),
    ).toBeInTheDocument();

    fireEvent.click(screen.getByRole("button", { name: "Next row" }));
    fireEvent.click(screen.getByRole("tab", { name: "JSON" }));
    const json = await screen.findByTestId("json-view");
    expect(JSON.parse(json.textContent ?? "")).toEqual({ id: 2, name: "renamed", note: null });
    expect(calls("row_get_form")).toContainEqual({ runId: "run-1", resultIndex: 0, row: 1 });
  });

  it("sends the filter and sort built in the builders", async () => {
    await renderPanel();
    await waitFor(() => expect(calls("edit_begin")).toHaveLength(1));
    fireEvent.click(screen.getByRole("button", { name: /Filter & sort/ }));
    const filter = screen.getByTestId("filter-builder");

    fireEvent.click(within(filter).getByRole("button", { name: "+ Condition" }));
    fireEvent.change(within(filter).getByRole("combobox", { name: "Column" }), {
      target: { value: "id" },
    });
    fireEvent.change(within(filter).getByRole("combobox", { name: "Operator" }), {
      target: { value: "between" },
    });
    fireEvent.change(within(filter).getByRole("textbox", { name: "Value 1" }), {
      target: { value: "1" },
    });
    // An incomplete condition is marked and nothing is sent.
    fireEvent.click(screen.getByRole("button", { name: "Apply filter" }));
    expect(screen.getByText("Fix the marked conditions first.")).toBeInTheDocument();
    expect(opens).toHaveLength(1);
    fireEvent.change(within(filter).getByRole("textbox", { name: "Value 2" }), {
      target: { value: "10" },
    });

    fireEvent.click(within(filter).getByRole("button", { name: "+ Group" }));
    const group = within(filter).getByRole("group", { name: "Group" });
    fireEvent.click(within(group).getByRole("checkbox", { name: "NOT" }));
    fireEvent.click(within(group).getByRole("button", { name: "+ Raw SQL" }));
    expect(within(group).getByText("RAW SQL")).toBeInTheDocument();
    fireEvent.change(within(group).getByRole("textbox", { name: "Raw SQL condition" }), {
      target: { value: "note LIKE 'x%'" },
    });

    const sort = screen.getByTestId("sort-builder");
    fireEvent.click(within(sort).getByRole("button", { name: "+ Sort key" }));
    fireEvent.change(within(sort).getByRole("combobox", { name: "Sort column 1" }), {
      target: { value: "name" },
    });
    fireEvent.change(within(sort).getByRole("combobox", { name: "Direction 1" }), {
      target: { value: "desc" },
    });
    fireEvent.change(within(sort).getByRole("combobox", { name: "NULLs 1" }), {
      target: { value: "last" },
    });

    fireEvent.click(screen.getByRole("button", { name: "Apply filter" }));
    await waitFor(() => expect(opens).toHaveLength(2));
    expect(opens[1]?.request).toEqual({
      table: TABLE,
      filter: {
        type: "and",
        items: [
          {
            type: "condition",
            column: "id",
            op: "between",
            values: [
              { type: "text", value: "1" },
              { type: "text", value: "10" },
            ],
          },
          { type: "not", item: { type: "or", items: [{ type: "rawSql", sql: "note LIKE 'x%'" }] } },
        ],
      },
      sort: [{ column: "name", descending: true, nulls: "last" }],
    });
    expect(opens[1]?.editorId).toBe("editor-t1");
  });

  it("sends a raw WHERE as typed", async () => {
    await renderPanel();
    fireEvent.click(screen.getByRole("button", { name: /Filter & sort/ }));
    fireEvent.click(screen.getByRole("radio", { name: "Raw WHERE" }));
    expect(screen.getAllByText("RAW SQL").length).toBeGreaterThan(0);
    fireEvent.change(screen.getByRole("textbox", { name: "Raw WHERE clause" }), {
      target: { value: "name = 'a''b'" },
    });
    fireEvent.click(screen.getByRole("button", { name: "Apply filter" }));
    await waitFor(() => expect(opens).toHaveLength(2));
    expect(opens[1]?.request).toEqual({
      table: TABLE,
      filter: { type: "rawSql", sql: "name = 'a''b'" },
      sort: [],
    });
  });

  it("asks before a new filter drops pending changes", async () => {
    await renderPanel();
    fireEvent.click(await screen.findByRole("button", { name: "Add row" }));
    fireEvent.click(screen.getByRole("button", { name: "Refresh" }));
    const dialog = await screen.findByRole("dialog", { name: "Discard pending changes?" });
    fireEvent.click(within(dialog).getByRole("button", { name: "Keep editing" }));
    expect(opens).toHaveLength(1);
    fireEvent.click(screen.getByRole("button", { name: "Refresh" }));
    await act(async () =>
      fireEvent.click(
        within(screen.getByRole("dialog")).getByRole("button", { name: "Discard changes" }),
      ),
    );
    await waitFor(() => expect(opens).toHaveLength(2));
  });
});
