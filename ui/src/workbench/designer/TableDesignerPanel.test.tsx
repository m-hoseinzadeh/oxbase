import { fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import type {
  DesignApplyReport,
  DesignerCapabilities,
  DesignPreview,
  EngineCapabilities,
  ObjectRef,
  TableDef,
  TableDesign,
} from "../../api";
import { tauriCore } from "../../test/tauriMock";
import type { SqlEditorProps } from "../editor/SqlEditor";
import { NO_DESIGNER } from "./designModel";
import { TableDesignerPanel } from "./TableDesignerPanel";

vi.mock("@tauri-apps/api/core", async () => (await import("../../test/tauriMock")).tauriCore);

// Monaco needs a real browser: the read-only viewer shows its text.
vi.mock("../editor/SqlEditor", () => ({
  SqlEditor: ({ value, readOnly, testId }: SqlEditorProps) => (
    <pre data-testid={testId} data-readonly={readOnly ? "yes" : "no"}>
      {value}
    </pre>
  ),
}));

const TABLE: ObjectRef = { kind: "table", catalog: "app", schema: "public", name: "items" };

const PG_DESIGNER: DesignerCapabilities = {
  ...NO_DESIGNER,
  transactionalDdl: true,
  createTable: true,
  dropTable: true,
  renameTable: "native",
  tableComment: "native",
  addColumn: "native",
  dropColumn: "native",
  renameColumn: "native",
  alterColumnType: "native",
  alterColumnNullability: "native",
  alterColumnDefault: "native",
  columnComment: "native",
  columnCollation: "native",
  reorderColumns: "unsupported",
  identity: true,
  generatedStored: true,
  changePrimaryKey: "native",
  addIndex: "native",
  dropIndex: "native",
  uniqueConstraints: true,
  partialIndexes: true,
  expressionIndexes: true,
  descendingIndexes: true,
  addForeignKey: "native",
  dropForeignKey: "native",
  fkActions: ["no_action", "restrict", "cascade", "set_null", "set_default"],
  addCheck: "native",
  dropCheck: "native",
  checksEnforced: true,
  maxIdentifierLength: 63,
};

function caps(designer: DesignerCapabilities, engine: EngineCapabilities["engine"] = "postgres") {
  return {
    engine,
    serverVersion: { major: 18, minor: 0, patch: 0, build: 0, raw: "18.0" },
    designer,
    gridEditing: true,
    nullsOrdering: true,
  } satisfies EngineCapabilities;
}

const ORIGINAL: TableDef = {
  ref: TABLE,
  columns: [
    {
      name: "id",
      data_type: { type: "int", bits: 64, unsigned: false },
      native_type: "bigint",
      nullable: false,
      auto_increment: false,
      extras: {},
    },
    {
      name: "qty",
      data_type: { type: "int", bits: 32, unsigned: false },
      native_type: "integer",
      nullable: true,
      default: "0",
      auto_increment: false,
      extras: {},
    },
  ],
  primary_key: { name: "items_pkey", columns: ["id"] },
  indexes: [
    {
      name: "items_qty_idx",
      columns: [{ name: "qty", is_expression: false, descending: false }],
      unique: false,
      is_constraint: false,
      extras: {},
    },
  ],
  foreign_keys: [],
  checks: [{ name: "qty_positive", expression: "qty >= 0", extras: {} }],
  extras: {},
};

const LOADED: TableDesign = {
  original: ORIGINAL,
  table: TABLE,
  columns: ORIGINAL.columns.map((column, origin) => ({ origin, column })),
  primaryKey: ORIGINAL.primary_key as NonNullable<TableDesign["primaryKey"]>,
  indexes: ORIGINAL.indexes.map((def, origin) => ({ origin, def })),
  foreignKeys: [],
  checks: ORIGINAL.checks.map((def, origin) => ({ origin, def })),
  extras: {},
};

/** `items` with a foreign key to itself and a two-column unique index. */
const WIDE: TableDef = {
  ...ORIGINAL,
  columns: [
    ...ORIGINAL.columns,
    {
      name: "parent_id",
      data_type: { type: "int", bits: 64, unsigned: false },
      native_type: "bigint",
      nullable: true,
      auto_increment: false,
      extras: {},
    },
  ],
  indexes: [
    ...ORIGINAL.indexes,
    {
      name: "items_qty_parent_key",
      columns: [
        { name: "qty", is_expression: false, descending: false },
        { name: "parent_id", is_expression: false, descending: false },
      ],
      unique: true,
      is_constraint: true,
      extras: {},
    },
  ],
  foreign_keys: [
    {
      name: "items_parent_fk",
      columns: ["parent_id"],
      ref_table: TABLE,
      ref_columns: ["id"],
      on_delete: "cascade",
      on_update: "no_action",
      extras: {},
    },
  ],
};

const WIDE_LOADED: TableDesign = {
  ...LOADED,
  original: WIDE,
  columns: WIDE.columns.map((column, origin) => ({ origin, column })),
  indexes: WIDE.indexes.map((def, origin) => ({ origin, def })),
  foreignKeys: WIDE.foreign_keys.map((def, origin) => ({ origin, def })),
};

function preview(patch: Partial<DesignPreview> = {}): DesignPreview {
  return {
    diffs: [
      {
        object: { kind: "column", parent: "items", name: "id" },
        prop: "name",
        kind: "changed",
        old: { type: "text", value: "id" },
        new: { type: "text", value: "item_id" },
      },
    ],
    script: {
      steps: [
        {
          sql: 'ALTER TABLE "public"."items" RENAME COLUMN "id" TO "item_id"',
          description: "rename column id to item_id",
          destructive: false,
          transactional: true,
          phase: "main",
          check: "none",
        },
      ],
    },
    sql: 'BEGIN;\n-- rename column id to item_id\nALTER TABLE "public"."items" RENAME COLUMN "id" TO "item_id";\nCOMMIT;',
    atomic: true,
    destructive: false,
    issues: [],
    notes: [],
    ...patch,
  };
}

let capabilities: EngineCapabilities;
let nextPreview: () => DesignPreview;
let applyResult: () => DesignApplyReport;
let loadDesign: (table: ObjectRef) => TableDesign;
let previewGate: Promise<void> | null;

function calls(cmd: string): Array<Record<string, unknown>> {
  return tauriCore.invoke.mock.calls
    .filter(([c]) => c === cmd)
    .map(([, a]) => (a ?? {}) as Record<string, unknown>);
}

beforeEach(() => {
  tauriCore.invoke.mockReset();
  tauriCore.isTauri.mockReturnValue(true);
  capabilities = caps(PG_DESIGNER);
  nextPreview = () => preview();
  applyResult = () => ({
    outcome: {
      committed: true,
      atomic: true,
      steps: [{ index: 0, status: "done", elapsedMs: 3 }],
      warnings: [],
    },
    design: LOADED,
  });
  loadDesign = (table) => {
    if (table.name !== TABLE.name) throw { category: "not_found", message: `${table.name}?` };
    return LOADED;
  };
  previewGate = null;
  tauriCore.invoke.mockImplementation(async (cmd, args) => {
    const a = (args ?? {}) as Record<string, unknown>;
    switch (cmd) {
      case "capabilities_get":
        return capabilities;
      case "designer_load":
        return loadDesign(a.table as ObjectRef);
      case "designer_preview":
        if (previewGate) await previewGate;
        return nextPreview();
      case "designer_apply":
        return applyResult();
      case "cancel_request":
        return null;
      default:
        throw new Error(`unexpected ${cmd}`);
    }
  });
});

/** Types a column name one keystroke at a time, then leaves the field (which commits it). */
function renameColumnInput(column: number, name: string, commit: "blur" | "enter" = "blur") {
  const input = screen.getByRole("textbox", { name: `Column ${column} name` });
  for (let i = 1; i <= name.length; i += 1) {
    fireEvent.change(input, { target: { value: name.slice(0, i) } });
  }
  if (commit === "blur") fireEvent.blur(input);
  else fireEvent.keyDown(input, { key: "Enter" });
}

async function renderDesigner(
  props: { isNew?: boolean; table?: ObjectRef; onTableChanged?: (table: ObjectRef) => void } = {},
) {
  render(
    <TableDesignerPanel
      sessionId="s-1"
      table={props.table ?? TABLE}
      isNew={props.isNew}
      previewDelayMs={0}
      onTableChanged={props.onTableChanged}
    />,
  );
  if (!props.isNew) await screen.findByRole("table", { name: "Columns" });
  await waitFor(() => expect(calls("capabilities_get")).toHaveLength(1));
  await waitFor(() =>
    expect(screen.getByRole("button", { name: "+ Column" })).not.toHaveAttribute("title"),
  );
}

describe("TableDesignerPanel", () => {
  it("asks for a preview after a change, keeping origins so a rename stays a rename", async () => {
    await renderDesigner();
    expect(calls("designer_load")).toEqual([{ sessionId: "s-1", table: TABLE, requestId: null }]);
    expect(calls("designer_preview")).toHaveLength(0);

    renameColumnInput(1, "item_id");
    await waitFor(() => expect(calls("designer_preview")).toHaveLength(1));
    const sent = calls("designer_preview")[0] as { sessionId: string; design: TableDesign };
    expect(sent.sessionId).toBe("s-1");
    expect(sent.design.original).toEqual(ORIGINAL);
    expect(sent.design.columns.map((c) => [c.origin, c.column.name])).toEqual([
      [0, "item_id"],
      [1, "qty"],
    ]);
    // References follow the rename.
    expect(sent.design.primaryKey?.columns).toEqual(["item_id"]);
    expect(sent.design.indexes[0]?.origin).toBe(0);
    expect(sent.design.checks[0]?.origin).toBe(0);

    const viewer = await screen.findByTestId("ddl-preview");
    await waitFor(() => expect(viewer).toHaveTextContent('RENAME COLUMN "id" TO "item_id"'));
    expect(viewer).toHaveAttribute("data-readonly", "yes");
    fireEvent.click(screen.getByRole("tab", { name: /Changes/ }));
    expect(screen.getByRole("list", { name: "Changes" })).toHaveTextContent(
      "~ items.id name: id → item_id",
    );

    // Applying a non-destructive script still asks first.
    fireEvent.click(screen.getByRole("button", { name: "Apply" }));
    const dialog = screen.getByRole("dialog", { name: "Apply the changes?" });
    expect(dialog).toHaveTextContent("1 statement will run on items in one transaction.");
    fireEvent.click(within(dialog).getByRole("button", { name: "Apply" }));
    await screen.findByText("Applied 1 statement.");
    expect(calls("designer_apply")).toEqual([
      { sessionId: "s-1", design: sent.design, options: null, requestId: null },
    ]);
    // The reloaded design replaces the edited one.
    expect(screen.getByRole("textbox", { name: "Column 1 name" })).toHaveValue("id");
  });

  it("asks for an explicit acknowledgement before a destructive apply", async () => {
    nextPreview = () =>
      preview({
        destructive: true,
        script: {
          steps: [
            {
              sql: 'ALTER TABLE "public"."items" DROP COLUMN "qty"',
              description: "drop column qty",
              destructive: true,
              transactional: true,
              phase: "main",
              check: "none",
            },
          ],
        },
        sql: 'ALTER TABLE "public"."items" DROP COLUMN "qty";',
      });
    await renderDesigner();
    fireEvent.click(screen.getByRole("button", { name: "Remove column 2" }));
    await waitFor(() => expect(calls("designer_preview")).toHaveLength(1));
    const sent = calls("designer_preview")[0] as { design: TableDesign };
    expect(sent.design.columns.map((c) => c.origin)).toEqual([0]);
    // The dropped column takes its index with it.
    expect(sent.design.indexes).toEqual([]);

    expect(await screen.findByTestId("destructive-warning")).toHaveTextContent("drop column qty");
    fireEvent.click(screen.getByRole("button", { name: "Apply" }));
    const dialog = screen.getByRole("dialog", { name: "Apply the changes?" });
    const confirm = within(dialog).getByRole("button", { name: "Apply destructive changes" });
    expect(confirm).toBeDisabled();
    fireEvent.click(within(dialog).getByRole("checkbox", { name: /data can be lost/ }));
    expect(confirm).toBeEnabled();
    fireEvent.click(confirm);
    await waitFor(() => expect(calls("designer_apply")).toHaveLength(1));
    expect(calls("designer_apply")[0]?.options).toEqual({ confirmDestructive: true });
  });

  it("warns when the script is not transactional and blocks apply on errors", async () => {
    capabilities = caps({ ...PG_DESIGNER, transactionalDdl: false }, "mysql");
    nextPreview = () =>
      preview({
        atomic: false,
        script: {
          steps: [
            {
              sql: "ALTER TABLE `items` ADD COLUMN `note` text",
              description: "add column note",
              destructive: false,
              transactional: false,
              phase: "main",
              check: "none",
            },
          ],
        },
        notes: ["MySQL commits each DDL statement; a failure leaves earlier statements applied."],
      });
    await renderDesigner();
    fireEvent.click(screen.getByRole("button", { name: "+ Column" }));
    const warning = await screen.findByTestId("non-atomic-warning");
    expect(warning).toHaveTextContent("Not transactional");
    expect(warning).toHaveTextContent("a failure leaves earlier statements applied");
    expect(screen.getByRole("button", { name: "Apply" })).toBeEnabled();

    nextPreview = () =>
      preview({
        issues: [{ severity: "error", path: "columns[2].name", message: "a column needs a name" }],
        script: { steps: [] },
        sql: "",
      });
    fireEvent.change(screen.getByRole("combobox", { name: "Column 3 type" }), {
      target: { value: "text" },
    });
    await waitFor(() => expect(calls("designer_preview")).toHaveLength(2));
    expect(await screen.findByRole("list", { name: "Issues" })).toHaveTextContent(
      "a column needs a name",
    );
    expect(screen.getByTestId("column-row-2")).toHaveClass("designer__row--error");
    expect(screen.getByRole("button", { name: "Apply" })).toBeDisabled();
    const sent = calls("designer_preview")[1] as { design: TableDesign };
    expect(sent.design.columns[2]).toEqual({
      column: {
        name: "",
        data_type: { type: "text" },
        native_type: "text",
        nullable: true,
        auto_increment: false,
        extras: {},
      },
    });
  });

  it("disables what the engine cannot do", async () => {
    capabilities = caps(
      {
        ...PG_DESIGNER,
        renameColumn: "unsupported",
        columnComment: "unsupported",
        dropColumn: "rebuild",
        addCheck: "unsupported",
        dropCheck: "unsupported",
      },
      "sqlite",
    );
    await renderDesigner();
    const name = screen.getByRole("textbox", { name: "Column 1 name" });
    expect(name).toBeDisabled();
    expect(name).toHaveAttribute("title", "Renaming a column is not supported by this database");
    expect(screen.getByRole("textbox", { name: "Column 1 comment" })).toBeDisabled();
    expect(screen.getByRole("button", { name: "Remove column 1" })).toHaveAttribute(
      "title",
      "Dropping a column rebuilds the table (data is copied)",
    );
    expect(screen.getByRole("button", { name: "Move id down" })).toBeDisabled();

    // A new column can be named.
    fireEvent.click(screen.getByRole("button", { name: "+ Column" }));
    expect(screen.getByRole("textbox", { name: "Column 3 name" })).toBeEnabled();

    fireEvent.click(screen.getByRole("tab", { name: /Checks/ }));
    expect(screen.getByRole("button", { name: "+ Check" })).toBeDisabled();
    expect(screen.getByRole("textbox", { name: "Check 1 expression" })).toBeDisabled();
  });

  it("designs a new table and creates it", async () => {
    const schema: ObjectRef = { kind: "table", catalog: "app", schema: "public", name: "" };
    await renderDesigner({ isNew: true, table: schema });
    expect(calls("designer_load")).toHaveLength(0);
    fireEvent.click(screen.getByRole("tab", { name: "Options" }));
    fireEvent.change(screen.getByRole("textbox", { name: "Table name" }), {
      target: { value: "orders" },
    });
    fireEvent.click(screen.getByRole("tab", { name: /Columns/ }));
    fireEvent.click(screen.getByRole("button", { name: "+ Column" }));
    renameColumnInput(1, "id");
    fireEvent.change(screen.getByRole("combobox", { name: "Column 1 type" }), {
      target: { value: "bigint" },
    });
    fireEvent.click(screen.getByRole("checkbox", { name: "Column 1 primary key" }));
    fireEvent.change(screen.getByRole("combobox", { name: "Column 1 default kind" }), {
      target: { value: "value" },
    });
    fireEvent.change(screen.getByRole("textbox", { name: "Column 1 default" }), {
      target: { value: "it's" },
    });
    await waitFor(() => {
      const last = calls("designer_preview").at(-1) as { design: TableDesign } | undefined;
      expect(last?.design).toEqual({
        table: { ...schema, name: "orders" },
        columns: [
          {
            column: {
              name: "id",
              data_type: { type: "int", bits: 64, unsigned: false },
              native_type: "bigint",
              nullable: false,
              auto_increment: false,
              extras: {},
            },
            defaultLiteral: "it's",
          },
        ],
        primaryKey: { columns: ["id"] },
        indexes: [],
        foreignKeys: [],
        checks: [],
        extras: {},
      });
    });
    await waitFor(() => expect(screen.getByRole("button", { name: "Create table" })).toBeEnabled());
  });

  it("shows step outcomes when an apply fails", async () => {
    applyResult = () => ({
      outcome: {
        committed: false,
        atomic: true,
        steps: [
          {
            index: 0,
            status: "failed",
            error: { category: "sql", message: 'column "id" does not exist' },
            elapsedMs: 1,
          },
        ],
        warnings: [],
      },
    });
    await renderDesigner();
    renameColumnInput(1, "item_id");
    await waitFor(() => expect(screen.getByRole("button", { name: "Apply" })).toBeEnabled());
    fireEvent.click(screen.getByRole("button", { name: "Apply" }));
    fireEvent.click(within(screen.getByRole("dialog")).getByRole("button", { name: "Apply" }));
    const error = await screen.findByTestId("designer-apply-error");
    expect(error).toHaveTextContent("rolled back");
    expect(error).toHaveTextContent(
      'failed · rename column id to item_id · column "id" does not exist',
    );
    // The edited design stays for another try.
    expect(screen.getByRole("textbox", { name: "Column 1 name" })).toHaveValue("item_id");
  });

  it("renames references once, from the committed name, never from a half-typed one", async () => {
    await renderDesigner();
    // Typing "id2" over qty passes through "i" and "id" (the PK column's name).
    renameColumnInput(2, "id2");
    await waitFor(() => expect(calls("designer_preview")).toHaveLength(1));
    const sent = calls("designer_preview")[0] as { design: TableDesign };
    expect(sent.design.columns.map((c) => [c.origin, c.column.name])).toEqual([
      [0, "id"],
      [1, "id2"],
    ]);
    // The primary key stays on id; the index follows qty.
    expect(sent.design.primaryKey?.columns).toEqual(["id"]);
    expect(sent.design.indexes[0]?.def.columns.map((c) => c.name)).toEqual(["id2"]);

    // A taken name is refused with the reason; nothing changes.
    renameColumnInput(2, "id", "enter");
    const name = screen.getByRole("textbox", { name: "Column 2 name" });
    expect(name).toHaveAttribute("aria-invalid", "true");
    expect(screen.getByRole("alert")).toHaveTextContent("Another column is already named id");
    // An empty name is refused too (references would point at nothing).
    fireEvent.change(name, { target: { value: "" } });
    fireEvent.blur(name);
    expect(screen.getByRole("alert")).toHaveTextContent("A column needs a name");
    // Escape drops the draft.
    fireEvent.keyDown(name, { key: "Escape" });
    expect(name).toHaveValue("id2");
    expect(name).not.toHaveAttribute("aria-invalid");
    expect(calls("designer_preview")).toHaveLength(1);
  });

  it("renames the referenced columns of a foreign key to the same table", async () => {
    loadDesign = () => WIDE_LOADED;
    await renderDesigner();
    renameColumnInput(1, "item_id");
    await waitFor(() => expect(calls("designer_preview")).toHaveLength(1));
    const sent = calls("designer_preview")[0] as { design: TableDesign };
    expect(sent.design.primaryKey?.columns).toEqual(["item_id"]);
    expect(sent.design.foreignKeys[0]?.def).toMatchObject({
      columns: ["parent_id"],
      ref_columns: ["item_id"],
    });
  });

  it("removes a composite index or foreign key with its column instead of narrowing it", async () => {
    loadDesign = () => WIDE_LOADED;
    await renderDesigner();
    fireEvent.click(screen.getByRole("button", { name: "Remove column 3" }));
    await waitFor(() => expect(calls("designer_preview")).toHaveLength(1));
    const sent = calls("designer_preview")[0] as { design: TableDesign };
    expect(sent.design.columns.map((c) => c.column.name)).toEqual(["id", "qty"]);
    // UNIQUE (qty, parent_id) goes whole; the single-column index on qty stays.
    expect(sent.design.indexes.map((i) => i.def.name)).toEqual(["items_qty_idx"]);
    expect(sent.design.foreignKeys).toEqual([]);
    expect(sent.design.primaryKey?.columns).toEqual(["id"]);
    expect(screen.getByTestId("column-removed-note")).toHaveTextContent(
      "Removing parent_id also removed unique index items_qty_parent_key, foreign key items_parent_fk.",
    );

    // Removing the referenced column drops the self-referencing foreign key too.
    fireEvent.click(screen.getByRole("button", { name: "Discard" }));
    fireEvent.click(screen.getByRole("button", { name: "Remove column 1" }));
    await waitFor(() => expect(calls("designer_preview")).toHaveLength(2));
    const second = calls("designer_preview")[1] as { design: TableDesign };
    expect(second.design.primaryKey).toBeUndefined();
    expect(second.design.foreignKeys).toEqual([]);
  });

  it("drops a preview that lands after the changes were discarded", async () => {
    let release: () => void = () => {};
    previewGate = new Promise((resolve) => {
      release = resolve;
    });
    await renderDesigner();
    renameColumnInput(1, "item_id");
    await waitFor(() => expect(calls("designer_preview")).toHaveLength(1));
    const requestId = calls("designer_preview")[0]?.requestId;
    fireEvent.click(screen.getByRole("button", { name: "Discard" }));
    expect(calls("cancel_request")).toContainEqual({ requestId });
    release();
    await new Promise((resolve) => setTimeout(resolve, 10));
    expect(screen.getByTestId("ddl-preview")).toHaveTextContent("-- No changes");
    expect(screen.queryByTestId("non-atomic-warning")).not.toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Apply" })).toBeDisabled();
  });

  it("reloads the renamed table and moves the tab to its new name", async () => {
    const renamed: ObjectRef = { ...TABLE, name: "goods" };
    const renamedDesign: TableDesign = {
      ...LOADED,
      original: { ...ORIGINAL, ref: renamed },
      table: renamed,
    };
    applyResult = () => ({
      outcome: {
        committed: true,
        atomic: true,
        steps: [{ index: 0, status: "done", elapsedMs: 3 }],
        warnings: [],
      },
      design: renamedDesign,
    });
    loadDesign = (table) => (table.name === "goods" ? renamedDesign : LOADED);
    const onTableChanged = vi.fn();
    await renderDesigner({ onTableChanged });
    fireEvent.click(screen.getByRole("tab", { name: "Options" }));
    fireEvent.change(screen.getByRole("textbox", { name: "Table name" }), {
      target: { value: "goods" },
    });
    await waitFor(() => expect(screen.getByRole("button", { name: "Apply" })).toBeEnabled());
    fireEvent.click(screen.getByRole("button", { name: "Apply" }));
    fireEvent.click(within(screen.getByRole("dialog")).getByRole("button", { name: "Apply" }));
    await screen.findByText("Applied 1 statement.");
    expect(onTableChanged).toHaveBeenCalledWith(renamed);

    fireEvent.click(screen.getByRole("button", { name: "Reload" }));
    await waitFor(() => expect(calls("designer_load")).toHaveLength(2));
    expect(calls("designer_load")[1]?.table).toEqual(renamed);
    expect(screen.queryByText(/Could not load the table/)).not.toBeInTheDocument();
  });

  it("gates identity and auto increment of a stored column on altering it", async () => {
    capabilities = caps({ ...PG_DESIGNER, alterColumnType: "unsupported", autoIncrement: true });
    await renderDesigner();
    fireEvent.click(screen.getByRole("button", { name: "More options of column 1" }));
    const identity = screen.getByRole("combobox", { name: "Column 1 identity" });
    expect(identity).toBeDisabled();
    expect(identity.closest("label")).toHaveAttribute(
      "title",
      "Changing identity is not supported by this database",
    );
    expect(screen.getByRole("checkbox", { name: "Column 1 auto increment" })).toBeDisabled();
    // A new column may use them.
    fireEvent.click(screen.getByRole("button", { name: "+ Column" }));
    fireEvent.click(screen.getByRole("button", { name: "More options of column 3" }));
    expect(screen.getByRole("combobox", { name: "Column 3 identity" })).toBeEnabled();
  });
});
