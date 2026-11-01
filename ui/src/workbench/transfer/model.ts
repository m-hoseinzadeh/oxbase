/**
 * The import wizard's and export dialog's view model: formats and their default options, type
 * choices and labels, the column mapping and its validation, the request sent to `import_plan`
 * / `import_run`, and progress figures (rate, ETA). Pure functions only, tested on their own
 * (docs/m5-contracts.md §3, §5).
 */
import type {
  ColumnDef,
  ColumnMapping,
  ConversionOptions,
  DataType,
  Engine,
  ErrorPolicy,
  FileFilter,
  FileFormat,
  ImportRequest,
  ObjectRef,
  PreviewColumn,
  ReadOptions,
  TargetMode,
  TransactionPolicy,
  WriteOptions,
} from "../../api";

// --- formats --------------------------------------------------------------------------------

export type ImportFormat = Exclude<FileFormat, "sql_dump">;

export const IMPORT_FORMATS: readonly ImportFormat[] = ["csv", "json", "xlsx", "parquet"];
export const EXPORT_FORMATS: readonly FileFormat[] = ["csv", "json", "xlsx", "parquet", "sql_dump"];

export const FORMAT_LABEL: Record<FileFormat, string> = {
  csv: "CSV / delimited text",
  json: "JSON",
  xlsx: "Excel workbook (XLSX)",
  parquet: "Parquet",
  sql_dump: "SQL dump",
};

/** The file extension an export writes (the save dialog's suggested name). */
export function extensionFor(options: WriteOptions): string {
  switch (options.format) {
    case "csv":
      return options.delimiter === "\t" ? "tsv" : "csv";
    case "json":
      return options.layout === "lines" ? "ndjson" : "json";
    case "xlsx":
      return "xlsx";
    case "parquet":
      return "parquet";
    case "sql_dump":
      return "sql";
  }
}

/** Filters of the import file dialog: every readable format first, then each one. */
export function importFilters(): FileFilter[] {
  return [
    {
      name: "Data files",
      extensions: ["csv", "tsv", "txt", "json", "ndjson", "jsonl", "xlsx", "parquet"],
    },
    { name: "Delimited text", extensions: ["csv", "tsv", "txt"] },
    { name: "JSON", extensions: ["json", "ndjson", "jsonl"] },
    { name: "Excel workbook", extensions: ["xlsx"] },
    { name: "Parquet", extensions: ["parquet"] },
    { name: "All files", extensions: ["*"] },
  ];
}

function extensionOf(name: string): string {
  const dot = name.lastIndexOf(".");
  return dot <= 0 ? "" : name.slice(dot + 1).toLowerCase();
}

/** The read options a picked file starts with, from its extension (unknown: CSV). */
export function readOptionsFor(fileName: string): ReadOptions {
  switch (extensionOf(fileName)) {
    case "tsv":
      return { format: "csv", delimiter: "\t" };
    case "json":
      return { format: "json", layout: "auto" };
    case "ndjson":
    case "jsonl":
      return { format: "json", layout: "lines" };
    case "xlsx":
      return { format: "xlsx" };
    case "parquet":
      return { format: "parquet" };
    default:
      return { format: "csv", delimiter: "," };
  }
}

/** Default read options of a format (when the user switches format). */
export function defaultReadOptions(format: ImportFormat): ReadOptions {
  switch (format) {
    case "csv":
      return { format: "csv", delimiter: "," };
    case "json":
      return { format: "json", layout: "auto" };
    case "xlsx":
      return { format: "xlsx" };
    case "parquet":
      return { format: "parquet" };
  }
}

/** What keeps read options from being previewed (the core checks them again). */
export function readOptionsProblem(options: ReadOptions): string | null {
  if (options.format !== "csv") return null;
  const delimiter = options.delimiter ?? ",";
  if (delimiter.length !== 1) return "Enter a one-character delimiter.";
  if (delimiter === (options.quote === undefined ? '"' : options.quote)) {
    return "The delimiter must differ from the quote character.";
  }
  if (options.formats?.decimalSeparator === delimiter) {
    return "The decimal separator must differ from the delimiter.";
  }
  return null;
}

/** Default write options of a format; a SQL dump targets `engine` (or PostgreSQL). */
export function defaultWriteOptions(format: FileFormat, engine?: Engine): WriteOptions {
  switch (format) {
    case "csv":
      return {
        format: "csv",
        delimiter: ",",
        header: true,
        quoteStyle: "necessary",
        encoding: "utf8",
        bom: false,
        lineEnding: "lf",
        nullToken: "",
        formulaGuard: true,
      };
    case "json":
      return {
        format: "json",
        layout: "array",
        pretty: false,
        binary: "base64",
        explicitNulls: true,
      };
    case "xlsx":
      return { format: "xlsx", sheetName: "Sheet1", header: true, formulaGuard: true };
    case "parquet":
      return { format: "parquet", compression: { codec: "snappy" } };
    case "sql_dump":
      return {
        format: "sql_dump",
        target: engine && SQL_DUMP_ENGINES.includes(engine) ? engine : "postgres",
        includeCreate: true,
        includeDrop: false,
        rowsPerInsert: 100,
      };
  }
}

/** Engines a SQL dump can target. */
export const SQL_DUMP_ENGINES: readonly Engine[] = [
  "postgres",
  "mysql",
  "mariadb",
  "sqlite",
  "mssql",
];

const SINGLE_BYTE = new Set(["windows1252", "latin1"]);

/** What is wrong with export options before they are sent (the core checks them again). */
export function writeOptionsProblems(options: WriteOptions): string[] {
  const problems: string[] = [];
  switch (options.format) {
    case "csv":
      if (options.delimiter?.length !== 1) {
        problems.push("The delimiter must be one character.");
      }
      if (options.bom && SINGLE_BYTE.has(options.encoding ?? "utf8")) {
        problems.push("A byte order mark needs a Unicode encoding.");
      }
      break;
    case "xlsx": {
      const name = options.sheetName ?? "Sheet1";
      if (name.length === 0 || name.length > 31) {
        problems.push("A sheet name has 1 to 31 characters.");
      } else if (/[[\]:*?/\\]/.test(name)) {
        problems.push("A sheet name cannot contain [ ] : * ? / or \\.");
      }
      break;
    }
    case "parquet": {
      const c = options.compression;
      if (c && "level" in c && c.level !== undefined) {
        const [min, max] = LEVEL_RANGE[c.codec] ?? [0, 0];
        if (!Number.isInteger(c.level) || c.level < min || c.level > max) {
          problems.push(`The ${c.codec} level must be between ${min} and ${max}.`);
        }
      }
      break;
    }
    case "sql_dump": {
      const n = options.rowsPerInsert ?? 100;
      if (!Number.isInteger(n) || n < 1 || n > 10_000) {
        problems.push("Rows per INSERT must be between 1 and 10,000.");
      }
      if (options.table && options.table.name.trim() === "") {
        problems.push("Enter the table name the dump inserts into.");
      }
      break;
    }
    case "json":
      break;
  }
  return problems;
}

/** Compression levels each Parquet codec accepts. */
export const LEVEL_RANGE: Partial<Record<string, [number, number]>> = {
  gzip: [0, 9],
  brotli: [0, 11],
  zstd: [1, 22],
};

// --- types ----------------------------------------------------------------------------------

/** A stable key of a type, for selects and comparisons. */
export function typeKey(type: DataType): string {
  return JSON.stringify(type);
}

function sized(name: string, n: number | null): string {
  return n === null ? name : `${name}(${n})`;
}

/** A short, engine-neutral name of a type (`bigint`, `decimal(12, 2)`, `varchar(255)`). */
export function typeLabel(type: DataType): string {
  switch (type.type) {
    case "bool":
      return "boolean";
    case "int": {
      const base =
        type.bits <= 8
          ? "tinyint"
          : type.bits <= 16
            ? "smallint"
            : type.bits <= 32
              ? "integer"
              : "bigint";
      return type.unsigned ? `${base} unsigned` : base;
    }
    case "float":
      return type.bits <= 32 ? "real" : "double";
    case "decimal":
      if (type.precision === null) return "decimal";
      return type.scale === null
        ? `decimal(${type.precision})`
        : `decimal(${type.precision}, ${type.scale})`;
    case "char":
      return sized("char", type.length);
    case "var_char":
      return sized("varchar", type.length);
    case "text":
      return "text";
    case "binary":
      return sized("binary", type.length);
    case "var_binary":
      return sized("varbinary", type.length);
    case "date":
      return "date";
    case "time":
      return "time";
    case "timestamp":
      return "timestamp";
    case "timestamp_tz":
      return "timestamp with time zone";
    case "interval":
      return "interval";
    case "uuid":
      return "uuid";
    case "json":
      return "json";
    case "array":
      return `${typeLabel(type.element)}[]`;
    case "enum":
      return "enum";
    case "other":
      return type.name || "other";
  }
}

/** Types offered for a column, besides the one inferred for it. */
export const TYPE_CHOICES: readonly DataType[] = [
  { type: "text" },
  { type: "var_char", length: 255 },
  { type: "int", bits: 32, unsigned: false },
  { type: "int", bits: 64, unsigned: false },
  { type: "decimal", precision: null, scale: null },
  { type: "decimal", precision: 38, scale: 10 },
  { type: "float", bits: 64 },
  { type: "bool" },
  { type: "date" },
  { type: "time", precision: 6 },
  { type: "timestamp", precision: 6 },
  { type: "timestamp_tz", precision: 6 },
  { type: "uuid" },
  { type: "json", binary: false },
  { type: "var_binary", length: null },
];

/**
 * The choices for a column: its inferred type first, then `extra` (the target column's type,
 * the current choice when it was sized by hand), then the common ones; no type twice.
 */
export function typeChoices(inferred: DataType, ...extra: DataType[]): DataType[] {
  const seen = new Set<string>();
  const out: DataType[] = [];
  for (const t of [inferred, ...extra, ...TYPE_CHOICES]) {
    const key = typeKey(t);
    if (seen.has(key)) continue;
    seen.add(key);
    out.push(t);
  }
  return out;
}

/** A size typed by the user: a whole number from `min`, or `null` (blank: unsized). */
export function parseSize(text: string, min: number): number | null {
  const t = text.trim();
  if (t === "") return null;
  const n = Number(t);
  return Number.isInteger(n) && n >= min ? n : null;
}

/** What is wrong with a hand-sized type (the core checks the engine's own limits). */
export function typeSizeProblem(type: DataType): string | null {
  if (type.type === "decimal") {
    if (type.scale !== null && type.precision === null) {
      return "A decimal scale needs a precision.";
    }
    if (type.precision !== null && type.scale !== null && type.scale > type.precision) {
      return "A decimal scale cannot be larger than its precision.";
    }
  }
  return null;
}

/**
 * The type an existing target column is read as. Text-like columns (char, varchar, text, enum,
 * engine-specific types) take the file's text as it is, so a value that does not look like the
 * rest of the sample is not lost and nothing is rewritten in a canonical form; other columns
 * are read as their own logical type.
 */
export function readTypeFor(column: ColumnDef): DataType {
  switch (column.data_type.type) {
    case "char":
    case "var_char":
    case "text":
    case "enum":
    case "other":
      return { type: "text" };
    default:
      return column.data_type;
  }
}

// --- names ----------------------------------------------------------------------------------

/** A table name suggested from a file name: lower case letters, digits and `_`. */
export function tableNameFromFile(fileName: string): string {
  const dot = fileName.lastIndexOf(".");
  const stem = dot > 0 ? fileName.slice(0, dot) : fileName;
  const name = stem
    .normalize("NFKD")
    .replace(/[̀-ͯ]/g, "")
    .toLowerCase()
    .replace(/[^a-z0-9_]+/g, "_")
    .replace(/^_+|_+$/g, "");
  if (!name) return "imported";
  // A suggestion only: the target engine's own limit is checked by `import_plan`.
  return (/^[0-9]/.test(name) ? `t_${name}` : name).slice(0, 63);
}

/** `schema.name` (or `catalog.name`) for messages. */
export function qualifiedName(table: ObjectRef): string {
  return [table.schema ?? table.catalog, table.name].filter(Boolean).join(".");
}

// --- target ---------------------------------------------------------------------------------

/** Modes that build the table from the mapping (column names and types come from the wizard). */
export function createsTable(mode: TargetMode): boolean {
  return mode === "create" || mode === "replace";
}

export const MODE_LABEL: Record<TargetMode, string> = {
  create: "Create a new table",
  append: "Append to the existing rows",
  truncate: "Empty the table first (TRUNCATE)",
  delete_all: "Delete all rows first (DELETE)",
  replace: "Replace the table (drop and create it again)",
};

// --- mapping --------------------------------------------------------------------------------

/**
 * A mapping row pointed at another existing column: the file is read as that column's type
 * (`readTypeFor`); blank or unknown targets keep the current type.
 */
export function retarget(target: string, targetColumns: ColumnDef[]): Partial<MappingRow> {
  const column = targetColumns.find((c) => c.name === target);
  return column ? { target, dataType: readTypeFor(column) } : { target };
}

/** One source column in the mapping table. */
export interface MappingRow {
  source: number;
  sourceName: string;
  /** The type the preview inferred. */
  inferred: DataType;
  /** The type the file is read as (and, when creating, the new column's type). */
  dataType: DataType;
  /** Target column; `""` = not chosen yet. */
  target: string;
  skip: boolean;
  /** Create / replace: part of the new table's primary key. */
  primaryKey: boolean;
}

/** The mapping a preview starts with: every column kept; existing tables matched by name. */
export function initialMapping(
  columns: PreviewColumn[],
  mode: TargetMode,
  targetColumns: ColumnDef[] = [],
): MappingRow[] {
  const byName = new Map(targetColumns.map((c) => [c.name.toLowerCase(), c]));
  return columns.map((c) => {
    let target = c.name;
    let skip = false;
    let dataType = c.dataType;
    if (!createsTable(mode)) {
      const column = byName.get(c.name.toLowerCase());
      target = column?.name ?? "";
      skip = target === "";
      if (column) dataType = readTypeFor(column);
    }
    return {
      source: c.index,
      sourceName: c.name,
      inferred: c.dataType,
      dataType,
      target,
      skip,
      primaryKey: false,
    };
  });
}

export interface MappingProblem {
  /** Source column index the problem is about (absent: the whole mapping). */
  source?: number;
  message: string;
}

function writeRefusal(column: ColumnDef): string | null {
  if (column.generated) return `"${column.name}" is a generated column and cannot be written.`;
  if (column.identity?.always) {
    return `"${column.name}" is an identity column (GENERATED ALWAYS) and cannot be written.`;
  }
  return null;
}

/** Whether the server fills `column` when the import leaves it out. */
function fillsItself(column: ColumnDef): boolean {
  return (
    column.nullable ||
    column.default !== undefined ||
    column.auto_increment ||
    column.identity !== undefined ||
    column.generated !== undefined
  );
}

/**
 * What stops the mapping from being imported, in table order. `targetColumns` are the existing
 * table's columns (append / truncate / delete_all).
 */
export function mappingProblems(
  rows: MappingRow[],
  mode: TargetMode,
  targetColumns: ColumnDef[] = [],
): MappingProblem[] {
  const problems: MappingProblem[] = [];
  const used = rows.filter((r) => !r.skip);
  if (used.length === 0) {
    problems.push({ message: "Import at least one column." });
    return problems;
  }
  const create = createsTable(mode);
  const existing = new Map(targetColumns.map((c) => [c.name, c]));
  const seen = new Map<string, MappingRow[]>();
  for (const row of used) {
    const target = row.target.trim();
    if (!target) {
      problems.push({
        source: row.source,
        message: create
          ? `Name the new column for "${row.sourceName}".`
          : `Choose a target column for "${row.sourceName}", or skip it.`,
      });
      continue;
    }
    // Names compare case-insensitively: most engines would refuse or merge them.
    const key = create ? target.toLowerCase() : target;
    seen.set(key, [...(seen.get(key) ?? []), row]);
    if (!create) {
      const column = existing.get(target);
      if (!column) {
        problems.push({ source: row.source, message: `"${target}" is not a column of the table.` });
        continue;
      }
      const refusal = writeRefusal(column);
      if (refusal) problems.push({ source: row.source, message: refusal });
    }
    // Name lengths are the target engine's rule: `import_plan` checks them.
    const sizeProblem = typeSizeProblem(row.dataType);
    if (sizeProblem) {
      problems.push({ source: row.source, message: `"${row.sourceName}": ${sizeProblem}` });
    }
  }
  for (const group of seen.values()) {
    if (group.length < 2) continue;
    const names = group.map((r) => `"${r.sourceName}"`).join(", ");
    const [first] = group;
    problems.push({
      ...(first ? { source: first.source } : {}),
      message: `${names} all go to "${first?.target.trim()}": each target column takes one source column.`,
    });
  }
  if (!create) {
    const mapped = new Set(used.map((r) => r.target.trim()));
    for (const column of targetColumns) {
      if (!mapped.has(column.name) && !fillsItself(column)) {
        problems.push({
          message: `"${column.name}" is NOT NULL and has no default: map a source column to it.`,
        });
      }
    }
  }
  return problems;
}

/** Everything the wizard collected, turned into an `ImportRequest`. */
export interface RequestInput {
  options: ReadOptions;
  rows: MappingRow[];
  session: string;
  table: ObjectRef;
  mode: TargetMode;
  conversion: ConversionOptions;
  errors: ErrorPolicy;
  transaction: TransactionPolicy;
}

export function buildImportRequest(input: RequestInput): ImportRequest {
  const create = createsTable(input.mode);
  const mapping: ColumnMapping[] = input.rows
    .filter((r) => !r.skip)
    .map((r) => ({
      source: r.source,
      target: r.target.trim(),
      ...(create ? { dataType: r.dataType } : {}),
    }));
  const primaryKey = create
    ? input.rows.filter((r) => !r.skip && r.primaryKey).map((r) => r.target.trim())
    : [];
  return {
    options: input.options,
    columns: input.rows.map((r) => ({ name: r.sourceName, dataType: r.dataType })),
    mapping,
    target: { session: input.session, table: input.table, mode: input.mode },
    ...(primaryKey.length > 0 ? { primaryKey } : {}),
    conversion: input.conversion,
    errors: input.errors,
    transaction: input.transaction,
  };
}

// --- progress -------------------------------------------------------------------------------

/** `950 B`, `12.3 KB`, `1.0 GB` (decimal units). */
export function formatBytes(n: number): string {
  if (n < 1000) return `${n} B`;
  const units = ["KB", "MB", "GB", "TB"];
  let value = n / 1000;
  let unit = 0;
  while (value >= 1000 && unit < units.length - 1) {
    value /= 1000;
    unit += 1;
  }
  return `${value.toFixed(value < 10 ? 2 : value < 100 ? 1 : 0)} ${units[unit]}`;
}

export function formatCount(n: number): string {
  return n.toLocaleString("en-US");
}

export interface Rates {
  /** 0..1, or `null` when the total is unknown. */
  fraction: number | null;
  rowsPerSecond: number | null;
  bytesPerSecond: number | null;
  /** Estimated time left, or `null` before there is a rate. */
  etaMs: number | null;
}

/**
 * Rates and time left from cumulative counters. `done` / `total` are what the fraction and the
 * ETA follow (bytes for an import, rows for an export with a known total).
 */
export function rates(input: {
  done: number;
  total: number | null | undefined;
  rows: number;
  bytes: number;
  elapsedMs: number;
}): Rates {
  const total = input.total ?? null;
  const fraction = total && total > 0 ? Math.min(1, input.done / total) : null;
  const seconds = input.elapsedMs / 1000;
  // Under half a second the rate is noise.
  if (seconds < 0.5) return { fraction, rowsPerSecond: null, bytesPerSecond: null, etaMs: null };
  const rowsPerSecond = input.rows / seconds;
  const bytesPerSecond = input.bytes / seconds;
  let etaMs: number | null = null;
  if (total && input.done > 0 && input.done < total) {
    etaMs = ((total - input.done) / input.done) * input.elapsedMs;
  } else if (total && input.done >= total) {
    etaMs = 0;
  }
  return { fraction, rowsPerSecond, bytesPerSecond, etaMs };
}
