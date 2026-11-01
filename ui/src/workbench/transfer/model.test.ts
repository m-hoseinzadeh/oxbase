import { describe, expect, it } from "vitest";
import type { ColumnDef, DataType, PreviewColumn } from "../../api";
import {
  buildImportRequest,
  extensionFor,
  formatBytes,
  initialMapping,
  type MappingRow,
  mappingProblems,
  rates,
  readOptionsFor,
  readOptionsProblem,
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
  });

  it("suggests table names from file names", () => {
    expect(tableNameFromFile("Orders 2026.csv")).toBe("orders_2026");
    expect(tableNameFromFile("café-menu.xlsx")).toBe("cafe_menu");
    expect(tableNameFromFile("2026.csv")).toBe("t_2026");
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
