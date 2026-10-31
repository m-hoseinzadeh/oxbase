/**
 * TypeScript mirrors of the M2 workbench payloads (docs/m2-contracts.md §5).
 *
 * Two naming conventions meet here, exactly as the Rust serde forms produce them:
 * - command wrapper types (tree nodes, run events, history) are camelCase;
 * - core model types embedded in them (`ObjectRef`, `ObjectSummary`, `ObjectDef`, `Value`,
 *   `DataType`) keep their snake_case serde form, which is also how they are persisted.
 *
 * Result rows never appear here: pages are Arrow IPC bytes decoded with `apache-arrow`.
 */
import type { Engine, ErrorCategory } from "../connections/types";

export type { Engine, ErrorCategory };

// --- core model (oxbase-types, snake_case) -------------------------------------------------

export type ObjectKind =
  | "database"
  | "schema"
  | "table"
  | "column"
  | "index"
  | "foreign_key"
  | "check"
  | "view"
  | "mat_view"
  | "function"
  | "procedure"
  | "trigger"
  | "sequence"
  | "user"
  | "role"
  | "grant"
  | "event"
  | "type"
  | "extension"
  | "key";

/** `oxbase_types::ObjectRef`: an engine-agnostic object path. Absent fields are `None`. */
export interface ObjectRef {
  kind: ObjectKind;
  catalog?: string;
  schema?: string;
  parent?: string;
  parent_kind?: ObjectKind;
  name: string;
  /** Routine overload discriminator (PostgreSQL identity arguments). */
  signature?: string;
}

/** `oxbase_types::Value` (`{ type, value }`). */
export type Value =
  | { type: "null" }
  | { type: "bool"; value: boolean }
  | { type: "int"; value: number }
  | { type: "u_int"; value: number }
  | { type: "float"; value: number | "NaN" | "Infinity" | "-Infinity" }
  | { type: "decimal"; value: string }
  | { type: "text"; value: string }
  | { type: "bytes"; value: number[] }
  | { type: "date"; value: string }
  | { type: "time"; value: string }
  | { type: "timestamp"; value: string }
  | { type: "timestamp_tz"; value: string }
  | { type: "uuid"; value: string }
  | { type: "json"; value: unknown };

/** `oxbase_types::Extras`: property id → value. */
export type Extras = Record<string, Value>;

/** `oxbase_types::DataType`. */
export type DataType =
  | { type: "bool" }
  | { type: "int"; bits: number; unsigned: boolean }
  | { type: "float"; bits: number }
  | { type: "decimal"; precision: number | null; scale: number | null }
  | { type: "char"; length: number | null }
  | { type: "var_char"; length: number | null }
  | { type: "text" }
  | { type: "binary"; length: number | null }
  | { type: "var_binary"; length: number | null }
  | { type: "date" }
  | { type: "time"; precision: number | null }
  | { type: "timestamp"; precision: number | null }
  | { type: "timestamp_tz"; precision: number | null }
  | { type: "interval" }
  | { type: "uuid" }
  | { type: "json"; binary: boolean }
  | { type: "array"; element: DataType }
  | { type: "enum"; labels: string[] }
  | { type: "other"; name: string };

/** `oxbase_types::ObjectSummary`: one listing row. */
export interface ObjectSummary {
  ref: ObjectRef;
  comment?: string;
  row_estimate?: number;
  system: boolean;
  extras?: Extras;
}

export interface IdentityDef {
  always: boolean;
  /** Decimal strings: may exceed a JS number. */
  start?: string;
  increment?: string;
}

export interface GeneratedColumn {
  expression: string;
  stored: boolean;
}

export interface ColumnDef {
  name: string;
  data_type: DataType;
  native_type: string;
  nullable: boolean;
  default?: string;
  auto_increment: boolean;
  identity?: IdentityDef;
  generated?: GeneratedColumn;
  collation?: string;
  comment?: string;
  extras: Extras;
}

export interface PrimaryKeyDef {
  name?: string;
  columns: string[];
}

export interface IndexColumn {
  name: string;
  is_expression: boolean;
  descending: boolean;
  length?: number;
}

export interface IndexDef {
  name: string;
  columns: IndexColumn[];
  unique: boolean;
  is_constraint: boolean;
  predicate?: string;
  extras: Extras;
}

export type FkAction = "no_action" | "restrict" | "cascade" | "set_null" | "set_default";

export interface ForeignKeyDef {
  name?: string;
  columns: string[];
  ref_table: ObjectRef;
  ref_columns: string[];
  on_delete: FkAction;
  on_update: FkAction;
  extras: Extras;
}

export interface CheckDef {
  name?: string;
  expression: string;
  extras: Extras;
}

export interface SchemaDef {
  ref: ObjectRef;
  owner?: string;
  comment?: string;
  extras: Extras;
}

export interface TableDef {
  ref: ObjectRef;
  columns: ColumnDef[];
  primary_key?: PrimaryKeyDef;
  indexes: IndexDef[];
  foreign_keys: ForeignKeyDef[];
  checks: CheckDef[];
  comment?: string;
  extras: Extras;
}

export interface ViewDef {
  ref: ObjectRef;
  definition: string;
  columns?: string[];
  comment?: string;
  extras: Extras;
}

export type ArgMode = "in" | "out" | "in_out" | "variadic" | "table";

export interface RoutineArg {
  name?: string;
  mode: ArgMode;
  native_type: string;
  default?: string;
}

export interface RoutineDef {
  ref: ObjectRef;
  args: RoutineArg[];
  returns?: string;
  language?: string;
  body?: string;
  definition?: string;
  comment?: string;
  extras: Extras;
}

export type TriggerTiming = "before" | "after" | "instead_of";
export type TriggerEvent = "insert" | "update" | "delete" | "truncate";

export interface TriggerDef {
  ref: ObjectRef;
  timing: TriggerTiming;
  events: TriggerEvent[];
  update_columns?: string[];
  level: "row" | "statement";
  condition?: string;
  function?: ObjectRef;
  body?: string;
  definition?: string;
  enabled: boolean;
  comment?: string;
  extras: Extras;
}

/** Numbers are decimal strings (they may exceed a JS number). */
export interface SequenceDef {
  ref: ObjectRef;
  data_type?: string;
  start: string;
  increment: string;
  min_value?: string;
  max_value?: string;
  cache?: string;
  cycle: boolean;
  owned_by?: ObjectRef;
  comment?: string;
  extras: Extras;
}

export interface GenericDef {
  ref: ObjectRef;
  definition?: string;
  comment?: string;
  extras: Extras;
}

/** `oxbase_types::ObjectDef`, tagged by `kind` (the variant, not the object kind). */
export type ObjectDef =
  | ({ kind: "schema" } & SchemaDef)
  | ({ kind: "table" } & TableDef)
  | { kind: "column"; table: ObjectRef; column: ColumnDef }
  | { kind: "index"; table: ObjectRef; index: IndexDef }
  | { kind: "foreign_key"; table: ObjectRef; foreign_key: ForeignKeyDef }
  | { kind: "check"; table: ObjectRef; check: CheckDef }
  | ({ kind: "view" } & ViewDef)
  | ({ kind: "routine" } & RoutineDef)
  | ({ kind: "trigger" } & TriggerDef)
  | ({ kind: "sequence" } & SequenceDef)
  | ({ kind: "generic" } & GenericDef);

// --- object tree (oxbase_services::tree, camelCase) -----------------------------------------

export type TreeNodeRef =
  | { type: "connection" }
  | { type: "folder"; parent: ObjectRef; kind: ObjectKind }
  | { type: "object"; object: ObjectRef };

export interface TreeNode {
  node: TreeNodeRef;
  key: string;
  label: string;
  expandable: boolean;
  summary?: ObjectSummary;
}

export interface TreeOptions {
  showSystem?: boolean;
}

// --- query runs (oxbase_services::query, camelCase) ------------------------------------------

/** A run id (UUID string). */
export type RunId = string;
/** An editor id chosen by the UI (1 to 200 bytes). */
export type EditorId = string;

export interface RunOptions {
  database?: string;
  maxRows?: number;
  stopOnError?: boolean;
  recordHistory?: boolean;
  keepPrevious?: boolean;
}

/** A statement of the script. Lines and columns are 1-based, columns in UTF-16 units. */
export interface StatementSpan {
  index: number;
  startByte: number;
  endByte: number;
  startLine: number;
  startColumn: number;
  endLine: number;
  endColumn: number;
  returnsRows: boolean;
}

export interface ColumnInfo {
  name: string;
  /** Arrow type as Arrow prints it (`Int64`, `Utf8`, ...). */
  dataType: string;
  nullable: boolean;
  metadata?: Record<string, string>;
}

export interface ErrorInfo {
  category: ErrorCategory;
  message: string;
}

export type NoticeLevel = "debug" | "info" | "warning";

/** `oxbase_services::query::RunEvent`. The last event is `finished`, `failed` or `cancelled`. */
export type RunEvent =
  | { type: "started"; runId: RunId; statements: StatementSpan[] }
  | { type: "statementStarted"; statementIndex: number }
  | {
      type: "resultSet";
      resultIndex: number;
      statementIndex: number;
      statementRange: StatementSpan;
      columns: ColumnInfo[];
    }
  | { type: "rows"; resultIndex: number; totalSoFar: number }
  | { type: "resultDone"; resultIndex: number; totalRows: number; truncated: boolean }
  | {
      type: "notice";
      statementIndex?: number;
      level: NoticeLevel;
      message: string;
      code?: string;
    }
  | { type: "statementDone"; statementIndex: number; rowsAffected?: number; elapsedMs: number }
  | { type: "statementFailed"; statementIndex: number; error: ErrorInfo; elapsedMs: number }
  | { type: "finished"; elapsedMs: number }
  | { type: "failed"; error: ErrorInfo; elapsedMs: number }
  | { type: "cancelled"; elapsedMs: number };

export function isTerminalEvent(event: RunEvent): boolean {
  return event.type === "finished" || event.type === "failed" || event.type === "cancelled";
}

// --- history (oxbase_store::history, camelCase) ----------------------------------------------

export type HistoryStatus = "ok" | "error" | "cancelled";

export interface HistoryEntry {
  id: string;
  profileId: string;
  database?: string;
  /** Already redacted (password literals are `'***'`). */
  sql: string;
  /** RFC 3339 UTC. */
  startedAt: string;
  durationMs: number;
  status: HistoryStatus;
  rows?: number;
  error?: string;
}

export interface HistoryQuery {
  profileId?: string;
  search?: string;
  /** RFC 3339; only entries that started before it. */
  before?: string;
  /** Default 200, at most 5000. */
  limit?: number;
}

export interface HistorySettings {
  enabled: boolean;
  /** 0 = unlimited. */
  maxEntries: number;
}
