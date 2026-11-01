/**
 * TypeScript mirrors of the workbench payloads: M2 (docs/m2-contracts.md §5) and M3 table data,
 * grid editing and the table designer (docs/m3-contracts.md §7).
 *
 * Two naming conventions meet here, exactly as the Rust serde forms produce them:
 * - command wrapper types (tree nodes, run events, history) are camelCase;
 * - core model types embedded in them (`ObjectRef`, `ObjectSummary`, `ObjectDef`, `Value`,
 *   `DataType`) keep their snake_case serde form, which is also how they are persisted.
 *
 * Result rows never appear here: pages are Arrow IPC bytes decoded with `apache-arrow`.
 */
import type { Engine, ErrorCategory, ServerVersion } from "../connections/types";

export type { Engine, ErrorCategory, ServerVersion };

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

// --- M3: filter and sort (oxbase_types::filter, camelCase) ----------------------------------

export type FilterOp =
  | "eq"
  | "ne"
  | "lt"
  | "le"
  | "gt"
  | "ge"
  | "like"
  | "notLike"
  | "contains"
  | "notContains"
  | "startsWith"
  | "endsWith"
  | "in"
  | "notIn"
  | "isNull"
  | "isNotNull"
  | "between"
  | "notBetween";

/**
 * `oxbase_types::Filter`. Values are typed `Value`s; the UI may send what the user typed as
 * `{ type: "text" }` values and the core converts them with the column's type.
 * `rawSql` is RAW SQL typed by the user (one boolean expression; no `;`, no comments).
 */
export type Filter =
  | { type: "and"; items: Filter[] }
  | { type: "or"; items: Filter[] }
  | { type: "not"; item: Filter }
  | { type: "condition"; column: string; op: FilterOp; values?: Value[] }
  | { type: "rawSql"; sql: string };

export type NullsOrder = "first" | "last";

export interface SortKey {
  column: string;
  descending?: boolean;
  nulls?: NullsOrder;
}

/** `SELECT * FROM table [WHERE filter] [ORDER BY sort] [LIMIT limit]`. */
export interface TableDataQuery {
  table: ObjectRef;
  filter?: Filter;
  sort?: SortKey[];
  limit?: number;
}

/** How many values each operator takes (mirrors `FilterOp::arity`). */
export function filterOpArity(op: FilterOp): "none" | "one" | "two" | "many" {
  switch (op) {
    case "isNull":
    case "isNotNull":
      return "none";
    case "between":
    case "notBetween":
      return "two";
    case "in":
    case "notIn":
      return "many";
    default:
      return "one";
  }
}

// --- M3: grid editing (oxbase_services::edit, camelCase) ------------------------------------

/** How the UI edits one column. */
export type InputKind =
  | { type: "text" }
  | { type: "integer" }
  | { type: "decimal" }
  | { type: "float" }
  | { type: "boolean" }
  | { type: "date" }
  | { type: "time" }
  | { type: "timestamp" }
  | { type: "timestampTz" }
  | { type: "uuid" }
  | { type: "json" }
  | { type: "bytes" }
  | { type: "enum"; labels: string[] }
  | { type: "other" };

/** `oxbase_types::RowKey` (core model, snake_case). */
export interface RowKey {
  table: ObjectRef;
  kind: "primary" | "unique";
  columns: string[];
}

export interface EditColumn {
  name: string;
  /** Position in the result. */
  index: number;
  dataType: DataType;
  nativeType: string;
  nullable: boolean;
  hasDefault: boolean;
  key: boolean;
  editable: boolean;
  readOnlyReason?: string;
  input: InputKind;
}

/** `edit_begin`: whether and how a table-data result can be edited. */
export interface Editability {
  runId: RunId;
  resultIndex: number;
  table: ObjectRef;
  editable: boolean;
  readOnlyReason?: string;
  key?: RowKey;
  columns: EditColumn[];
  /**
   * `false` when the table's writes cannot be rolled back (MySQL / MariaDB `MyISAM`, ...): an
   * apply that stops keeps, and reports `applied`, the edits that ran before it.
   */
  transactional: boolean;
  /** Shown with the grid (for example why a failed apply may still have saved changes). */
  warning?: string;
}

/** What the user typed into a cell: NULL, the column default (new rows only), or text. */
export type CellInput = { type: "null" } | { type: "default" } | { type: "text"; text: string };

export interface CellEdit {
  column: string;
  input: CellInput;
}

/** One unsaved edit. `row` is a grid row number. */
export type PendingEdit =
  | { op: "update"; row: number; cells: CellEdit[] }
  | { op: "insert"; cells: CellEdit[] }
  | { op: "delete"; row: number };

/** The UI's buffer of unsaved edits, sent whole to `edit_apply` (helpers in `./pending`). */
export interface PendingChanges {
  edits: PendingEdit[];
}

export type EditStatus = "applied" | "conflict" | "failed" | "rolledBack" | "notRun";

export interface EditOutcome {
  status: EditStatus;
  error?: ErrorInfo;
  /** The grid row; for an applied insert, its new row number. */
  row?: number;
  /** The row read back, as edit texts in result-column order (`null` = NULL). */
  values?: (string | null)[];
  warning?: string;
}

export interface ApplyReport {
  /** Every edit was applied. `false` with some `applied` only on a non-transactional table. */
  committed: boolean;
  /** One per pending edit, same order. */
  outcomes: EditOutcome[];
}

export interface FormField {
  column: string;
  /** Edit text; absent means NULL. */
  text?: string;
}

/** `row_get_form`: one row for the form / JSON view. */
export interface FormRow {
  row: number;
  fields: FormField[];
  /** Pretty-printed JSON object of the row. */
  json: string;
}

// --- M3: table designer (oxbase_designer / oxbase_services::designer, camelCase) -----------

export type Support = "native" | "rebuild" | "unsupported";

/** `oxbase_dialect::DesignerCapabilities`. */
export interface DesignerCapabilities {
  transactionalDdl: boolean;
  createTable: boolean;
  dropTable: boolean;
  renameTable: Support;
  tableComment: Support;
  addColumn: Support;
  dropColumn: Support;
  renameColumn: Support;
  alterColumnType: Support;
  alterColumnNullability: Support;
  alterColumnDefault: Support;
  columnComment: Support;
  columnCollation: Support;
  reorderColumns: Support;
  autoIncrement: boolean;
  identity: boolean;
  generatedStored: boolean;
  generatedVirtual: boolean;
  changePrimaryKey: Support;
  addIndex: Support;
  dropIndex: Support;
  uniqueConstraints: boolean;
  partialIndexes: boolean;
  expressionIndexes: boolean;
  descendingIndexes: boolean;
  indexPrefixLengths: boolean;
  indexMethods: string[];
  addForeignKey: Support;
  dropForeignKey: Support;
  fkActions: FkAction[];
  deferrableConstraints: boolean;
  addCheck: Support;
  dropCheck: Support;
  checksEnforced: boolean;
  maxIdentifierLength: number | null;
  notes: string[];
}

/** `capabilities_get`. */
export interface EngineCapabilities {
  engine: Engine;
  serverVersion: ServerVersion;
  designer: DesignerCapabilities;
  gridEditing: boolean;
  nullsOrdering: boolean;
}

export interface DesignColumn {
  /** Index in `original.columns`; absent for a new column. */
  origin?: number;
  column: ColumnDef;
  /** A default typed as a plain value; the dialect quotes it. */
  defaultLiteral?: string;
}

export interface DesignItem<T> {
  origin?: number;
  def: T;
}

/** `oxbase_designer::TableDesign`: the designer's editable model, sent back whole. */
export interface TableDesign {
  /** The table as loaded; absent for a new table. Send it back unchanged. */
  original?: TableDef;
  table: ObjectRef;
  columns: DesignColumn[];
  primaryKey?: PrimaryKeyDef;
  indexes: DesignItem<IndexDef>[];
  foreignKeys: DesignItem<ForeignKeyDef>[];
  checks: DesignItem<CheckDef>[];
  comment?: string;
  extras: Extras;
}

/** An empty design for a new table (`TableDesign::new_table`). */
export function newTableDesign(table: ObjectRef): TableDesign {
  return { table, columns: [], indexes: [], foreignKeys: [], checks: [], extras: {} };
}

export type DiffKind = "added" | "removed" | "changed";

/** `oxbase_types::PropertyDiff` (core model, snake_case). */
export interface PropertyDiff {
  object: ObjectRef;
  /** Property id (`name`, `data_type`, `nullable`, ...); absent for a whole object. */
  prop?: string;
  kind: DiffKind;
  old?: Value;
  new?: Value;
}

/** `oxbase_types::ScriptStep` (core model, snake_case). */
export interface ScriptStep {
  sql: string;
  description?: string;
  destructive: boolean;
  transactional: boolean;
  phase: "prepare" | "main" | "finally";
  check: "none" | "expect_no_rows";
  capture?: string;
  only_if?: { capture: string; equals: string };
}

export interface Script {
  steps: ScriptStep[];
  warnings?: string[];
}

export interface DesignIssue {
  severity: "error" | "warning";
  /** Path into the design: `columns[2].name`, `checks[0].expression`, ... */
  path: string;
  message: string;
}

/** `designer_preview`. */
export interface DesignPreview {
  diffs: PropertyDiff[];
  script: Script;
  sql: string;
  atomic: boolean;
  destructive: boolean;
  issues: DesignIssue[];
  notes: string[];
}

export interface DesignApplyOptions {
  /** The user confirmed a destructive script. */
  confirmDestructive?: boolean;
}

export type StepStatus = "done" | "failed" | "skipped" | "rolledBack" | "notRun";

export interface StepOutcome {
  index: number;
  status: StepStatus;
  error?: ErrorInfo;
  elapsedMs: number;
}

export interface ScriptOutcome {
  committed: boolean;
  atomic: boolean;
  steps: StepOutcome[];
  warnings: string[];
}

/** `designer_apply`. */
export interface DesignApplyReport {
  outcome: ScriptOutcome;
  /** The table as it is now (reloaded). */
  design?: TableDesign;
}
