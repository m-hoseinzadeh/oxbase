import { fireEvent, render, screen } from "@testing-library/react";
import { useState } from "react";
import { describe, expect, it, vi } from "vitest";
import type { DesignerCapabilities, ServerVersion, TableDef, TableDesign } from "../../api";
import { ColumnsEditor, IndexesEditor, OptionsEditor } from "./DesignerTabs";
import {
  columnLockReason,
  designTraits,
  guessDataType,
  indexLockReason,
  NO_DESIGNER,
  newColumn,
  tableReadOnlyReason,
  typeSuggestions,
} from "./designModel";

/** The SQL Server designer capabilities of docs/m4-contracts.md §4.4. */
const MSSQL_DESIGNER: DesignerCapabilities = {
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
  autoIncrement: false,
  identity: true,
  generatedStored: true,
  generatedVirtual: true,
  changePrimaryKey: "native",
  addIndex: "native",
  dropIndex: "native",
  uniqueConstraints: true,
  partialIndexes: true,
  expressionIndexes: false,
  descendingIndexes: true,
  indexPrefixLengths: false,
  addForeignKey: "native",
  dropForeignKey: "native",
  fkActions: ["no_action", "cascade", "set_null", "set_default"],
  addCheck: "native",
  dropCheck: "native",
  checksEnforced: true,
  maxIdentifierLength: 128,
};

const V2022: ServerVersion = { major: 16, minor: 0, patch: 0, build: 0, raw: "16.0" };
const V2025: ServerVersion = { major: 17, minor: 0, patch: 0, build: 0, raw: "17.0" };

const ORIGINAL: TableDef = {
  ref: { kind: "table", catalog: "oxbase_test", schema: "m4", name: "orders" },
  columns: [
    {
      name: "id",
      data_type: { type: "int", bits: 32, unsigned: false },
      native_type: "int",
      nullable: false,
      auto_increment: true,
      identity: { always: true, start: "100", increment: "5" },
      extras: {},
    },
    {
      name: "ver",
      data_type: { type: "binary", length: 8 },
      native_type: "rowversion",
      nullable: false,
      auto_increment: false,
      extras: { row_version: { type: "bool", value: true } },
    },
    {
      name: "code",
      data_type: { type: "var_char", length: 20 },
      native_type: "nvarchar(20)",
      nullable: true,
      auto_increment: false,
      extras: {},
    },
  ],
  primary_key: { name: "PK_orders", columns: ["id"] },
  indexes: [
    {
      name: "ix_code",
      columns: [{ name: "code", is_expression: false, descending: false }],
      unique: true,
      is_constraint: false,
      predicate: "([code] IS NOT NULL)",
      extras: {
        clustered: { type: "bool", value: false },
        index_kind: { type: "text", value: "nonclustered" },
        include_columns: { type: "json", value: ["ver"] },
      },
    },
    {
      name: "ccs",
      columns: [],
      unique: false,
      is_constraint: false,
      extras: { index_kind: { type: "text", value: "nonclustered_columnstore" } },
    },
  ],
  foreign_keys: [],
  checks: [],
  extras: {},
};

function loaded(): TableDesign {
  return {
    original: ORIGINAL,
    table: ORIGINAL.ref,
    columns: ORIGINAL.columns.map((column, origin) => ({ origin, column })),
    primaryKey: { name: "PK_orders", columns: ["id"] },
    indexes: ORIGINAL.indexes.map((def, origin) => ({ origin, def })),
    foreignKeys: [],
    checks: [],
    extras: {},
  };
}

type Editor = typeof ColumnsEditor;

function Harness({
  editor: E,
  initial,
  onChange,
  serverVersion = V2025,
}: {
  editor: Editor;
  initial: TableDesign;
  onChange: (d: TableDesign) => void;
  serverVersion?: ServerVersion;
}) {
  const [design, setDesign] = useState(initial);
  return (
    <E
      design={design}
      onChange={(d) => {
        setDesign(d);
        onChange(d);
      }}
      caps={MSSQL_DESIGNER}
      issues={[]}
      engine="mssql"
      serverVersion={serverVersion}
      isNew={false}
      disabled={false}
    />
  );
}

describe("SQL Server designer model", () => {
  it("offers the SQL Server types, json from SQL Server 2025 on", () => {
    const types = typeSuggestions("mssql", V2022);
    for (const t of [
      "nvarchar(max)",
      "datetime2(7)",
      "decimal(18,2)",
      "uniqueidentifier",
      "rowversion",
      "datetimeoffset(7)",
      "money",
      "xml",
      "varbinary(max)",
    ]) {
      expect(types).toContain(t);
    }
    expect(types).not.toContain("json");
    expect(typeSuggestions("mssql", V2025)).toContain("json");
    // An unknown version counts as current.
    expect(typeSuggestions("mssql", { ...V2025, major: 0 })).toContain("json");
    expect(typeSuggestions("postgres", V2025)).not.toContain("uniqueidentifier");
    expect(typeSuggestions(undefined)).toEqual([]);
  });

  it("maps T-SQL type names to neutral types", () => {
    const g = (t: string) => guessDataType(t, "mssql");
    expect(g("nvarchar(max)")).toEqual({ type: "text" });
    expect(g("nvarchar(40)")).toEqual({ type: "var_char", length: 40 });
    expect(g("varbinary(max)")).toEqual({ type: "var_binary", length: null });
    expect(g("datetime2(3)")).toEqual({ type: "timestamp", precision: 3 });
    expect(g("datetime")).toEqual({ type: "timestamp", precision: 3 });
    expect(g("datetimeoffset")).toEqual({ type: "timestamp_tz", precision: 7 });
    expect(g("decimal(12,2)")).toEqual({ type: "decimal", precision: 12, scale: 2 });
    expect(g("money")).toEqual({ type: "decimal", precision: 19, scale: 4 });
    expect(g("uniqueidentifier")).toEqual({ type: "uuid" });
    expect(g("bit")).toEqual({ type: "bool" });
    expect(g("tinyint")).toEqual({ type: "int", bits: 8, unsigned: true });
    expect(g("float(24)")).toEqual({ type: "float", bits: 32 });
    expect(g("rowversion")).toEqual({ type: "binary", length: 8 });
    expect(g("xml")).toEqual({ type: "other", name: "xml" });
    expect(g("json")).toEqual({ type: "json", binary: true });
    // Elsewhere `timestamp` stays a timestamp.
    expect(guessDataType("timestamp", "postgres")).toEqual({ type: "timestamp", precision: null });
  });

  it("knows what SQL Server cannot change", () => {
    expect(designTraits("mssql")).toMatchObject({
      identityStyle: "seedIncrement",
      identityOnNewColumnsOnly: true,
      clusteredIndexes: true,
      includedColumns: true,
    });
    expect(designTraits("postgres").identityStyle).toBe("generated");
    const [id, ver] = ORIGINAL.columns;
    expect(id && columnLockReason(id)).toBeNull();
    expect(ver && columnLockReason(ver)).toMatch(/rowversion/);
    const [ix, ccs] = ORIGINAL.indexes;
    expect(ix && indexLockReason(ix, designTraits("mssql"))).toBeNull();
    expect(ccs && indexLockReason(ccs, designTraits("mssql"))).toMatch(/columnstore/);
    expect(tableReadOnlyReason(loaded())).toBeNull();
    const temporal = loaded();
    temporal.original = {
      ...ORIGINAL,
      extras: { temporal_type: { type: "text", value: "system_versioned" } },
    };
    expect(tableReadOnlyReason(temporal)).toMatch(/system-versioned/);
    temporal.original.extras = { memory_optimized: { type: "bool", value: true } };
    expect(tableReadOnlyReason(temporal)).toMatch(/memory-optimized/);
  });
});

describe("SQL Server designer editors", () => {
  it("offers the type list and locks rowversion columns", () => {
    render(<Harness editor={ColumnsEditor} initial={loaded()} onChange={vi.fn()} />);
    const list = document.querySelector("datalist");
    const options = Array.from(list?.querySelectorAll("option") ?? []).map((o) => o.value);
    expect(options).toContain("nvarchar(max)");
    expect(options).toContain("json");
    const verType = screen.getByRole("combobox", { name: "Column 2 type" });
    expect(verType).toBeDisabled();
    expect(verType).toHaveAttribute("title", expect.stringMatching(/rowversion/));
    expect(screen.getByRole("combobox", { name: "Column 3 type" })).toBeEnabled();
  });

  it("sets IDENTITY(seed, increment) on new columns only", () => {
    const onChange = vi.fn();
    const design = loaded();
    design.columns.push({
      column: { ...newColumn("n").column, native_type: "bigint", default: "((0))" },
    });
    render(<Harness editor={ColumnsEditor} initial={design} onChange={onChange} />);

    // The existing identity column shows its seed and increment, locked.
    fireEvent.click(screen.getByRole("button", { name: "More options of column 1" }));
    expect(screen.getByRole("checkbox", { name: "Column 1 identity" })).toBeChecked();
    expect(screen.getByRole("checkbox", { name: "Column 1 identity" })).toBeDisabled();
    expect(screen.getByRole("textbox", { name: "Column 1 identity seed" })).toHaveValue("100");
    expect(screen.getByRole("textbox", { name: "Column 1 identity increment" })).toBeDisabled();
    expect(screen.queryByRole("checkbox", { name: "Column 1 auto increment" })).toBeNull();
    expect(screen.queryByRole("combobox", { name: "Column 1 identity" })).toBeNull();

    // A new column cannot be a second identity column.
    fireEvent.click(screen.getByRole("button", { name: "More options of column 4" }));
    expect(screen.getByRole("checkbox", { name: "Column 4 identity" })).toBeDisabled();
  });

  it("makes a new column the identity with seed and increment", () => {
    const onChange = vi.fn();
    const design: TableDesign = {
      table: { kind: "table", schema: "m4", name: "t" },
      columns: [{ column: { ...newColumn("id").column, native_type: "int", default: "((0))" } }],
      indexes: [],
      foreignKeys: [],
      checks: [],
      extras: {},
    };
    render(<Harness editor={ColumnsEditor} initial={design} onChange={onChange} />);
    fireEvent.click(screen.getByRole("button", { name: "More options of column 1" }));
    fireEvent.click(screen.getByRole("checkbox", { name: "Column 1 identity" }));
    let column = onChange.mock.lastCall?.[0].columns[0].column;
    expect(column).toMatchObject({
      nullable: false,
      auto_increment: true,
      identity: { always: true, start: "1", increment: "1" },
    });
    expect(column.default).toBeUndefined();
    fireEvent.change(screen.getByRole("textbox", { name: "Column 1 identity seed" }), {
      target: { value: "1000" },
    });
    fireEvent.change(screen.getByRole("textbox", { name: "Column 1 identity increment" }), {
      target: { value: "0" },
    });
    column = onChange.mock.lastCall?.[0].columns[0].column;
    expect(column.identity).toEqual({ always: true, start: "1000", increment: "0" });
    expect(screen.getByRole("alert")).toHaveTextContent("other than 0");
    fireEvent.click(screen.getByRole("checkbox", { name: "Column 1 identity" }));
    column = onChange.mock.lastCall?.[0].columns[0].column;
    expect(column.identity).toBeUndefined();
    expect(column.auto_increment).toBe(false);
  });

  it("edits clustered, included columns and the filter of indexes", () => {
    const onChange = vi.fn();
    render(<Harness editor={IndexesEditor} initial={loaded()} onChange={onChange} />);
    expect(screen.getByRole("columnheader", { name: "Clustered" })).toBeInTheDocument();
    expect(screen.getByRole("textbox", { name: "Index 1 predicate" })).toHaveValue(
      "([code] IS NOT NULL)",
    );
    const include = screen.getByRole("textbox", { name: "Index 1 included columns" });
    expect(include).toHaveValue("ver");
    fireEvent.change(include, { target: { value: "ver, code" } });
    fireEvent.blur(include);
    expect(onChange.mock.lastCall?.[0].indexes[0].def.extras.include_columns).toEqual({
      type: "json",
      value: ["ver", "code"],
    });
    // Clearing them is written as an empty list, so the change is seen.
    fireEvent.change(include, { target: { value: "" } });
    fireEvent.blur(include);
    expect(onChange.mock.lastCall?.[0].indexes[0].def.extras.include_columns).toEqual({
      type: "json",
      value: [],
    });

    fireEvent.click(screen.getByRole("checkbox", { name: "Index 1 clustered" }));
    const extras = onChange.mock.lastCall?.[0].indexes[0].def.extras;
    expect(extras.clustered).toEqual({ type: "bool", value: true });
    expect(extras.index_kind).toEqual({ type: "text", value: "clustered" });
    // A clustered index has no included columns.
    expect(screen.getByRole("textbox", { name: "Index 1 included columns" })).toBeDisabled();

    // A columnstore index is kept as it is.
    expect(screen.getByRole("textbox", { name: "Index 2 name" })).toBeDisabled();
    expect(screen.getByText(/columnstore index is kept/)).toBeInTheDocument();
  });

  it("has no expression or prefix-length keys", () => {
    render(<Harness editor={IndexesEditor} initial={loaded()} onChange={vi.fn()} />);
    expect(screen.getByRole("textbox", { name: "Index 1 columns" })).toHaveAttribute(
      "placeholder",
      "a, b DESC",
    );
  });

  it("chooses whether the primary key is clustered", () => {
    const onChange = vi.fn();
    render(<Harness editor={OptionsEditor} initial={loaded()} onChange={onChange} />);
    const box = screen.getByRole("checkbox", { name: "Clustered primary key" });
    // Absent means clustered.
    expect(box).toBeChecked();
    fireEvent.click(box);
    expect(onChange.mock.lastCall?.[0].extras.clustered).toEqual({ type: "bool", value: false });
  });
});
