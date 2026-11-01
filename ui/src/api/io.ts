/**
 * M5 import / export: payload types (mirrors of `oxbase-io`, `oxbase_services::import` /
 * `export` and `crates/app-tauri/src/files.rs`) and typed wrappers of the commands in
 * `crates/app-tauri/src/{files,transfer}.rs` (docs/m5-contracts.md §6-7).
 *
 * Files are opaque handles from `filePickOpen` / `filePickSave`, which show the native dialog
 * from Rust; this module never sends a file-system path.
 */
import { Channel } from "@tauri-apps/api/core";
import { call } from "./call";
import type { DataType, Engine, ErrorInfo, ObjectRef, RunId, TableDataQuery } from "./types";

// --- file handles (camelCase) ---------------------------------------------------------------

/** Opaque handle of a file picked in a native dialog (valid for this app run). */
export type FileHandle = string;

export type FileAccess = "read" | "write";

/** A dialog filter: label and extensions without the dot (`["csv", "tsv"]`, or `["*"]`). */
export interface FileFilter {
  name: string;
  extensions: string[];
}

export interface PickedFile {
  handle: FileHandle;
  /** Base name only, for display. */
  name: string;
  access: FileAccess;
  /** Read handles: size in bytes. */
  size?: number;
  /** The file existed when picked (save: the user confirmed replacing it). */
  exists: boolean;
}

// --- formats (oxbase-io, camelCase fields, `format` / `codec` tags) --------------------------

export type FileFormat = "csv" | "json" | "xlsx" | "parquet" | "sql_dump";

export type TextEncoding = "auto" | "utf8" | "utf16le" | "utf16be" | "windows1252" | "latin1";

export type LineEnding = "auto" | "lf" | "cr_lf" | "cr";

export interface ValueFormats {
  dateFormat?: string;
  timeFormat?: string;
  timestampFormat?: string;
  /** "." or ",". */
  decimalSeparator: string;
  thousandsSeparator?: string;
  trueValues: string[];
  falseValues: string[];
}

export interface CsvReadOptions {
  delimiter: string;
  quote: string | null;
  escape: string | null;
  header: boolean;
  skipRows: number;
  encoding: TextEncoding;
  lineEnding: LineEnding;
  comment: string | null;
  nullTokens: string[];
  trim: boolean;
  allowRagged: boolean;
  formats: ValueFormats;
}

export type JsonLayout = "auto" | "array" | "lines";

export interface JsonReadOptions {
  layout: JsonLayout;
  encoding: TextEncoding;
  formats: ValueFormats;
}

export interface XlsxReadOptions {
  sheet?: string;
  /** 1-based header row; 0 = no header. */
  headerRow: number;
  formats: ValueFormats;
}

/** What to read. Every field but `format` may be left out (Rust fills defaults). */
export type ReadOptions =
  | ({ format: "csv" } & Partial<CsvReadOptions>)
  | ({ format: "json" } & Partial<JsonReadOptions>)
  | ({ format: "xlsx" } & Partial<XlsxReadOptions>)
  | { format: "parquet" };

export type QuoteStyle = "necessary" | "always" | "non_numeric" | "never";

export interface CsvWriteOptions {
  delimiter: string;
  quote: string;
  quoteStyle: QuoteStyle;
  escape: string | null;
  header: boolean;
  /** Not "auto". */
  encoding: TextEncoding;
  bom: boolean;
  /** "lf" or "cr_lf". */
  lineEnding: LineEnding;
  nullToken: string;
  formats: ValueFormats;
  formulaGuard: boolean;
}

export type BinaryEncoding = "base64" | "hex";

export interface JsonWriteOptions {
  /** "array" or "lines". */
  layout: JsonLayout;
  pretty: boolean;
  binary: BinaryEncoding;
  explicitNulls: boolean;
}

export interface XlsxWriteOptions {
  sheetName: string;
  header: boolean;
  /** Header included; at most 1 048 576. */
  maxRowsPerSheet: number;
  formulaGuard: boolean;
  dateFormat: string;
  timestampFormat: string;
}

export type ParquetCompression =
  | { codec: "none" }
  | { codec: "snappy" }
  | { codec: "gzip"; level?: number }
  | { codec: "brotli"; level?: number }
  | { codec: "lz4_raw" }
  | { codec: "zstd"; level?: number };

export interface ParquetWriteOptions {
  compression: ParquetCompression;
  rowGroupRows: number;
}

export interface SqlDumpOptions {
  target: Engine;
  targetVersion?: string;
  table?: ObjectRef;
  includeCreate?: boolean;
  includeDrop?: boolean;
  rowsPerInsert?: number;
}

/** What to write. Every field but `format` (and a dump's `target`) may be left out. */
export type WriteOptions =
  | ({ format: "csv" } & Partial<CsvWriteOptions>)
  | ({ format: "json" } & Partial<JsonWriteOptions>)
  | ({ format: "xlsx" } & Partial<XlsxWriteOptions>)
  | ({ format: "parquet" } & Partial<ParquetWriteOptions>)
  | ({ format: "sql_dump" } & SqlDumpOptions);

// --- preview -------------------------------------------------------------------------------

export interface Detected {
  encoding?: TextEncoding;
  bom: boolean;
  delimiter?: string;
  jsonLines?: boolean;
  sheets?: string[];
  sheet?: string;
  totalRows?: number;
}

export interface PreviewColumn {
  index: number;
  name: string;
  dataType: DataType;
  nullable: boolean;
  warnings?: string[];
}

export interface SourcePreview {
  columns: PreviewColumn[];
  /** First rows as display text; `null` = NULL. */
  rows: (string | null)[][];
  sampledRows: number;
  complete: boolean;
  totalBytes: number;
  detected: Detected;
  warnings?: string[];
}

export interface ColumnSpec {
  name: string;
  dataType: DataType;
}

// --- import (camelCase; DataType / ObjectRef keep their snake_case core form) --------------

export type ImportId = string;

export type TargetMode = "create" | "append" | "truncate" | "delete_all" | "replace";

export interface ImportTarget {
  session: string;
  table: ObjectRef;
  mode?: TargetMode;
}

export interface ColumnMapping {
  /** Index into `ImportRequest.columns`. */
  source: number;
  /** Target column; absent = skip. */
  target?: string;
  /** Create / replace: the new column's type. */
  dataType?: DataType;
}

export interface ConversionOptions {
  trimText?: boolean;
  emptyTextAsNull?: boolean;
  truncateText?: boolean;
}

export type ErrorPolicy = { type: "stop" } | { type: "skip"; maxErrors: number };

export type TransactionPolicy =
  | { type: "auto" }
  | { type: "single" }
  | { type: "batches"; rows: number };

export interface ImportRequest {
  options: ReadOptions;
  columns: ColumnSpec[];
  mapping: ColumnMapping[];
  target: ImportTarget;
  primaryKey?: string[];
  conversion?: ConversionOptions;
  errors?: ErrorPolicy;
  transaction?: TransactionPolicy;
}

export type BulkMethod =
  | "copy_text"
  | "copy_binary"
  | "multi_row_insert"
  | "prepared_insert"
  | "tds_bulk_load";

export interface PlannedColumn {
  source: number;
  sourceName: string;
  sourceType: DataType;
  target: string;
  targetType: DataType;
  conversion?: string;
}

export interface ImportPlan {
  table: ObjectRef;
  mode: TargetMode;
  createsTable: boolean;
  ddl: string[];
  columns: PlannedColumn[];
  method: BulkMethod;
  /** Absent `commitEvery`: one transaction. */
  transaction: { commitEvery?: number };
  destructive: boolean;
  warnings?: string[];
}

export interface ImportRowError {
  row: number;
  column?: string;
  message: string;
}

export interface ImportReport {
  importId: ImportId;
  table: ObjectRef;
  method: BulkMethod;
  rowsRead: number;
  rowsInserted: number;
  rowsCommitted: number;
  rowsSkipped: number;
  errors: ImportRowError[];
  errorsTruncated: boolean;
  tableCreated: boolean;
  tableDropped: boolean;
  leftBehind: string;
  bytesRead: number;
  elapsedMs: number;
  warnings?: string[];
}

export type ImportEvent =
  | { type: "started"; importId: ImportId; totalBytes: number; plan: ImportPlan }
  | {
      type: "progress";
      bytesRead: number;
      totalBytes: number;
      rowsRead: number;
      rowsInserted: number;
      rowsCommitted: number;
      rowsSkipped: number;
      elapsedMs: number;
    }
  | { type: "rowError"; error: ImportRowError }
  | { type: "committed"; rowsCommitted: number }
  | { type: "finished"; report: ImportReport }
  | { type: "failed"; error: ErrorInfo; report: ImportReport }
  | { type: "cancelled"; report: ImportReport };

// --- export ----------------------------------------------------------------------------------

export type ExportId = string;

export type ExportSource =
  | { type: "table"; session: string; query: TableDataQuery }
  | { type: "query"; session: string; sql: string; database?: string }
  | { type: "result"; runId: RunId; resultIndex: number };

export interface ExportRequest {
  source: ExportSource;
  options: WriteOptions;
}

export interface ExportReport {
  exportId: ExportId;
  fileName: string;
  rows: number;
  bytes: number;
  sheets?: string[];
  warnings?: string[];
  sourceTruncated: boolean;
  elapsedMs: number;
}

export type ExportEvent =
  | { type: "started"; exportId: ExportId; totalRows?: number }
  | { type: "progress"; rows: number; bytesWritten: number; totalRows?: number; elapsedMs: number }
  | { type: "finished"; report: ExportReport }
  | { type: "failed"; error: ErrorInfo; elapsedMs: number }
  | { type: "cancelled"; elapsedMs: number };

/** `true` for the last event of an import or export. */
export function isTerminalTransferEvent(event: ImportEvent | ExportEvent): boolean {
  return event.type === "finished" || event.type === "failed" || event.type === "cancelled";
}

/** Dialog filters for a format (first extension preferred). */
export function filtersFor(format: FileFormat): FileFilter[] {
  switch (format) {
    case "csv":
      return [{ name: "Delimited text", extensions: ["csv", "tsv", "txt"] }];
    case "json":
      return [{ name: "JSON", extensions: ["json", "ndjson", "jsonl"] }];
    case "xlsx":
      return [{ name: "Excel workbook", extensions: ["xlsx"] }];
    case "parquet":
      return [{ name: "Parquet", extensions: ["parquet"] }];
    case "sql_dump":
      return [{ name: "SQL", extensions: ["sql"] }];
  }
}

// --- commands --------------------------------------------------------------------------------

/** Native open dialog (shown by Rust). `null` when the user cancelled. */
export function filePickOpen(filters: FileFilter[], title?: string): Promise<PickedFile | null> {
  return call("file_pick_open", { filters, title: title ?? null });
}

/**
 * Native save dialog (shown by Rust) with `defaultName` (a plain file name) suggested. `null`
 * when the user cancelled. The handle is used up by `exportRun`.
 */
export function filePickSave(
  defaultName: string,
  filters: FileFilter[],
  title?: string,
): Promise<PickedFile | null> {
  return call("file_pick_save", { defaultName, filters, title: title ?? null });
}

/** Drops a handle early; `false` when it was unknown. */
export function fileRelease(handle: FileHandle): Promise<boolean> {
  return call("file_release", { handle });
}

/** Columns with inferred types, the first rows, what was detected. */
export function importPreview(
  file: FileHandle,
  options: ReadOptions,
  requestId?: string,
): Promise<SourcePreview> {
  return call("import_preview", { file, options, requestId: requestId ?? null });
}

/** What `importRun` would do with `request` (DDL, columns, method, policy). Changes nothing. */
export function importPlan(
  file: FileHandle,
  request: ImportRequest,
  requestId?: string,
): Promise<ImportPlan> {
  return call("import_plan", { file, request, requestId: requestId ?? null });
}

/** Starts an import; events arrive on `onEvent`. */
export function importRun(
  file: FileHandle,
  request: ImportRequest,
  onEvent: (event: ImportEvent) => void,
): Promise<ImportId> {
  const channel = new Channel<ImportEvent>();
  channel.onmessage = onEvent;
  return call("import_run", { file, request, onEvent: channel });
}

export function importCancel(importId: ImportId): Promise<boolean> {
  return call("import_cancel", { importId });
}

/** Starts an export into a file picked with `filePickSave`; events arrive on `onEvent`. */
export function exportRun(
  file: FileHandle,
  request: ExportRequest,
  onEvent: (event: ExportEvent) => void,
): Promise<ExportId> {
  const channel = new Channel<ExportEvent>();
  channel.onmessage = onEvent;
  return call("export_run", { file, request, onEvent: channel });
}

export function exportCancel(exportId: ExportId): Promise<boolean> {
  return call("export_cancel", { exportId });
}
