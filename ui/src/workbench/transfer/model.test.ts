import { describe, expect, it } from "vitest";
import type { ColumnDef, DataType, PreviewColumn } from "../../api";
import {
  buildImportRequest,
  extensionFor,
  formatBytes,
  initialMapping,
  type MappingRow,
  mappingProblems,
  parseSize,
  rates,
  readOptionsFor,
  readOptionsProblem,
  readTypeFor,
  retarget,
  tableNameFromFile,
  typeChoices,
  typeKey,
  typeLabel,
  writeOptionsProblems,
} from "./model";

const INT: DataType = { type: "int", bits: 64, unsigned: false };
const TEXT: DataType = { type: "text" };

function column(name: string, extra: Partial<ColumnDef> = {}): ColumnDef {
  return {
    name,
    data_type: TEXT,
    native_type: "text",
    nullable: true,
    auto_increment: false,
    extras: {},
    ...extra,
  };
}

const PREVIEW_COLUMNS: PreviewColumn[] = [
  { index: 0, name: "ID", dataType: INT, nullable: false },
  { index: 1, name: "note", dataType: TEXT, nullable: true },
  { index: 2, name: "extra", dataType: TEXT, nullable: true },
];

describe("formats", () => {
  it("starts read options from the file extension", () => {
    expect(readOptionsFor("orders.csv")).toEqual({ format: "csv", delimiter: "," });
    expect(readOptionsFor("ORDERS.TSV")).toEqual({ format: "csv", delimiter: "\t" });
    expect(readOptionsFor("a.ndjson")).toEqual({ format: "json", layout: "lines" });
    expect(readOptionsFor("a.json")).toEqual({ format: "json", layout: "auto" });
    expect(readOptionsFor("book.xlsx")).toEqual({ format: "xlsx" });
    expect(readOptionsFor("data.parquet")).toEqual({ format: "parquet" });
    expect(readOptionsFor("README")).toEqual({ format: "csv", delimiter: "," });
  });

  it("checks read options before a preview", () => {
    expect(readOptionsProblem({ format: "csv", delimiter: "" })).toMatch(/one-character/);
    expect(readOptionsProblem({ format: "csv", delimiter: '"' })).toMatch(/quote/);
    expect(readOptionsProblem({ format: "csv", delimiter: ";" })).toBeNull();
    expect(readOptionsProblem({ format: "parquet" })).toBeNull();
  });

  it("names export files by format", () => {
    expect(extensionFor({ format: "csv", delimiter: "\t" })).toBe("tsv");
    expect(extensionFor({ format: "json", layout: "lines" })).toBe("ndjson");
    expect(extensionFor({ format: "sql_dump", target: "mysql" })).toBe("sql");
  });

  it("checks export options", () => {
    expect(writeOptionsProblems({ format: "xlsx", sheetName: "a/b" })).toEqual([
      "A sheet name cannot contain [ ] : * ? / or \\.",
    ]);
    expect(writeOptionsProblems({ format: "xlsx", sheetName: "x".repeat(32) })).toEqual([
      "A sheet name has 1 to 31 characters.",
    ]);
    expect(
      writeOptionsProblems({ format: "csv", delimiter: ",", encoding: "latin1", bom: true }),
    ).toEqual(["A byte order mark needs a Unicode encoding."]);
    expect(
      writeOptionsProblems({ format: "parquet", compression: { codec: "zstd", level: 30 } }),
    ).toEqual(["The zstd level must be between 1 and 22."]);
    expect(
      writeOptionsProblems({ format: "sql_dump", target: "postgres", rowsPerInsert: 0 }),
    ).toEqual(["Rows per INSERT must be between 1 and 10,000."]);
    expect(writeOptionsProblems({ format: "json" })).toEqual([]);
  });
});

describe("types and names", () => {
  it("labels types engine-neutrally", () => {
    expect(typeLabel(INT)).toBe("bigint");
    expect(typeLabel({ type: "int", bits: 32, unsigned: true })).toBe("integer unsigned");
    expect(typeLabel({ type: "decimal", precision: 12, scale: 2 })).toBe("decimal(12, 2)");
    expect(typeLabel({ type: "var_char", length: 255 })).toBe("varchar(255)");
    expect(typeLabel({ type: "timestamp_tz", precision: 6 })).toBe("timestamp with time zone");
    expect(typeLabel({ type: "array", element: TEXT })).toBe("text[]");
  });

  it("offers the inferred type first, once", () => {
    const choices = typeChoices(TEXT);
    expect(choices[0]).toEqual(TEXT);
    expect(choices.filter((t) => typeKey(t) === typeKey(TEXT))).toHaveLength(1);
    const odd: DataType = { type: "decimal", precision: 7, scale: 3 };
    expect(typeChoices(odd)[0]).toEqual(odd);
    // The target column's type and a hand-sized choice are offered too, each once.
    const sized: DataType = { type: "decimal", precision: 20, scale: 12 };
    const withExtra = typeChoices(INT, TEXT, sized, INT).map(typeKey);
    expect(withExtra.slice(0, 3)).toEqual([typeKey(INT), typeKey(TEXT), typeKey(sized)]);
    expect(new Set(withExtra).size).toBe(withExtra.length);
    // An unsized decimal is a common choice.
    expect(typeChoices(TEXT).map(typeKey)).toContain(
      typeKey({ type: "decimal", precision: null, scale: null }),
    );
  });

  it("reads sizes typed by the user", () => {
    expect(parseSize("12", 1)).toBe(12);
    expect(parseSize(" ", 1)).toBeNull();
    expect(parseSize("0", 1)).toBeNull();
    expect(parseSize("0", 0)).toBe(0);
    expect(parseSize("2.5", 0)).toBeNull();
  });

  it("suggests table names from file names", () => {
    expect(tableNameFromFile("Orders 2026.csv")).toBe("orders_2026");
    expect(tableNameFromFile("café-menu.xlsx")).toBe("cafe_menu");
    expect(tableNameFromFile("2026.csv")).toBe("t_2026");
    expect(tableNameFromFile(`1${"a".repeat(80)}.csv`)).toHaveLength(63);
    expect(tableNameFromFile(`${"b".repeat(80)}.csv`)).toHaveLength(63);
    expect(tableNameFromFile("...csv")).toBe("imported");
  });

  it("formats sizes", () => {
    expect(formatBytes(999)).toBe("999 B");
    expect(formatBytes(12_345)).toBe("12.3 KB");
    expect(formatBytes(1_000_000_000)).toBe("1.00 GB");
  });
});

describe("mapping", () => {
  it("keeps every column for a new table", () => {
    const rows = initialMapping(PREVIEW_COLUMNS, "create");
    expect(rows.map((r) => [r.target, r.skip])).toEqual([
      ["ID", false],
      ["note", false],
      ["extra", false],
    ]);
    expect(mappingProblems(rows, "create")).toEqual([]);
  });

  it("matches existing columns by name, case-insensitively, and skips the rest", () => {
    const rows = initialMapping(PREVIEW_COLUMNS, "append", [column("id"), column("note")]);
    expect(rows.map((r) => [r.target, r.skip])).toEqual([
      ["id", false],
      ["note", false],
      ["", true],
    ]);
  });

  it("reads an existing column as its own type, text-like columns as text", () => {
    const table = [
      column("id", { data_type: { type: "var_char", length: 20 }, native_type: "varchar(20)" }),
      column("note", {
        data_type: { type: "decimal", precision: 10, scale: 2 },
        native_type: "numeric(10,2)",
      }),
    ];
    // "ID" looks like integers in the sample, but goes into a varchar column: read as text.
    const rows = initialMapping(PREVIEW_COLUMNS, "append", table);
    expect(rows[0]?.inferred).toEqual(INT);
    expect(rows[0]?.dataType).toEqual(TEXT);
    expect(rows[1]?.dataType).toEqual({ type: "decimal", precision: 10, scale: 2 });
    const request = buildImportRequest({
      options: { format: "csv" },
      rows,
      session: "s-1",
      table: { kind: "table", name: "t" },
      mode: "append",
      conversion: {},
      errors: { type: "stop" },
      transaction: { type: "auto" },
    });
    expect(request.columns[0]).toEqual({ name: "ID", dataType: TEXT });
    expect(readTypeFor(column("e", { data_type: { type: "enum", labels: ["a"] } }))).toEqual(TEXT);
    expect(readTypeFor(column("o", { data_type: { type: "other", name: "citext" } }))).toEqual(
      TEXT,
    );
    expect(readTypeFor(column("d", { data_type: { type: "date" } }))).toEqual({ type: "date" });
    // Picking another target column follows that column's type.
    expect(retarget("note", table)).toEqual({
      target: "note",
      dataType: { type: "decimal", precision: 10, scale: 2 },
    });
    expect(retarget("id", table)).toEqual({ target: "id", dataType: TEXT });
    expect(retarget("", table)).toEqual({ target: "" });
  });

  it("leaves name lengths to the engine and checks hand-sized decimals", () => {
    const rows = initialMapping(PREVIEW_COLUMNS, "create").map((r) =>
      r.source === 0 ? { ...r, target: "c".repeat(100) } : r,
    );
    expect(mappingProblems(rows, "create")).toEqual([]);
    const scaled = rows.map((r) =>
      r.source === 1
        ? { ...r, dataType: { type: "decimal", precision: 4, scale: 6 } as DataType }
        : r.source === 2
          ? { ...r, dataType: { type: "decimal", precision: null, scale: 2 } as DataType }
          : r,
    );
    expect(mappingProblems(scaled, "create")).toEqual([
      { source: 1, message: '"note": A decimal scale cannot be larger than its precision.' },
      { source: 2, message: '"extra": A decimal scale needs a precision.' },
    ]);
  });

  it("needs at least one column", () => {
    const rows = initialMapping(PREVIEW_COLUMNS, "create").map((r) => ({ ...r, skip: true }));
    expect(mappingProblems(rows, "create")).toEqual([{ message: "Import at least one column." }]);
  });

  it("names missing and duplicate target columns", () => {
    const rows: MappingRow[] = initialMapping(PREVIEW_COLUMNS, "create");
    rows[1] = { ...(rows[1] as MappingRow), target: " " };
    rows[2] = { ...(rows[2] as MappingRow), target: "id" };
    expect(mappingProblems(rows, "create")).toEqual([
      { source: 1, message: 'Name the new column for "note".' },
      {
        source: 0,
        message: '"ID", "extra" all go to "ID": each target column takes one source column.',
      },
    ]);
  });

  it("refuses generated and identity-always targets and unmapped NOT NULL columns", () => {
    const table = [
      column("id", { nullable: false, identity: { always: true } }),
      column("note", { generated: { expression: "x", stored: true } }),
      column("created_at", { nullable: false }),
      column("updated_at", { nullable: false, default: "now()" }),
      column("serial", { nullable: false, auto_increment: true }),
    ];
    const rows = initialMapping(PREVIEW_COLUMNS, "append", table);
    expect(mappingProblems(rows, "append", table).map((p) => p.message)).toEqual([
      '"id" is an identity column (GENERATED ALWAYS) and cannot be written.',
      '"note" is a generated column and cannot be written.',
      '"created_at" is NOT NULL and has no default: map a source column to it.',
    ]);
  });

  it("asks for a target or a skip in an existing table", () => {
    const table = [column("id")];
    const rows = initialMapping(PREVIEW_COLUMNS, "append", table).map((r) =>
      r.source === 2 ? { ...r, skip: false } : r,
    );
    expect(mappingProblems(rows, "append", table)).toEqual([
      { source: 2, message: 'Choose a target column for "extra", or skip it.' },
    ]);
    const unknown = rows.map((r) => (r.source === 2 ? { ...r, target: "gone" } : r));
    expect(mappingProblems(unknown, "append", table)).toEqual([
      { source: 2, message: '"gone" is not a column of the table.' },
    ]);
  });

  it("builds the request: types and key for a new table, only names for an existing one", () => {
    const rows = initialMapping(PREVIEW_COLUMNS, "create").map((r) =>
      r.source === 0
        ? { ...r, target: "id", primaryKey: true }
        : r.source === 2
          ? { ...r, skip: true }
          : { ...r, dataType: { type: "var_char", length: 255 } as DataType },
    );
    const base = {
      options: { format: "csv" as const, delimiter: ";" },
      rows,
      session: "s-1",
      table: { kind: "table" as const, schema: "public", name: "orders" },
      conversion: { trimText: true },
      errors: { type: "skip" as const, maxErrors: 5 },
      transaction: { type: "batches" as const, rows: 1000 },
    };
    expect(buildImportRequest({ ...base, mode: "create" })).toEqual({
      options: { format: "csv", delimiter: ";" },
      columns: [
        { name: "ID", dataType: INT },
        { name: "note", dataType: { type: "var_char", length: 255 } },
        { name: "extra", dataType: TEXT },
      ],
      mapping: [
        { source: 0, target: "id", dataType: INT },
        { source: 1, target: "note", dataType: { type: "var_char", length: 255 } },
      ],
      target: { session: "s-1", table: base.table, mode: "create" },
      primaryKey: ["id"],
      conversion: { trimText: true },
      errors: { type: "skip", maxErrors: 5 },
      transaction: { type: "batches", rows: 1000 },
    });
    const append = buildImportRequest({ ...base, mode: "append" });
    expect(append.mapping).toEqual([
      { source: 0, target: "id" },
      { source: 1, target: "note" },
    ]);
    expect(append.primaryKey).toBeUndefined();
  });
});

describe("rates", () => {
  it("estimates rate and time left from the counters", () => {
    expect(rates({ done: 250, total: 1000, rows: 10_000, bytes: 250, elapsedMs: 2000 })).toEqual({
      fraction: 0.25,
      rowsPerSecond: 5000,
      bytesPerSecond: 125,
      etaMs: 6000,
    });
  });

  it("waits for a meaningful sample and handles unknown totals", () => {
    expect(rates({ done: 10, total: 100, rows: 1, bytes: 10, elapsedMs: 100 })).toEqual({
      fraction: 0.1,
      rowsPerSecond: null,
      bytesPerSecond: null,
      etaMs: null,
    });
    const open = rates({ done: 50, total: null, rows: 50, bytes: 500, elapsedMs: 1000 });
    expect(open.fraction).toBeNull();
    expect(open.etaMs).toBeNull();
    expect(open.rowsPerSecond).toBe(50);
    expect(rates({ done: 10, total: 10, rows: 10, bytes: 10, elapsedMs: 1000 }).etaMs).toBe(0);
  });
});
