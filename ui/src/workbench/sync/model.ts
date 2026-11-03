/**
 * M8 view models (docs/m8-contracts.md §8.4): pure helpers behind the transfer wizard, the
 * structure and data sync views and the backup / restore dialogs. No React, no IPC.
 */
import type {
  BackupEvent,
  BackupProgress,
  BackupReport,
  ColumnOverride,
  CompareProgress,
  DataApplyEvent,
  DataApplyReport,
  DataCompareEvent,
  DataCompareResult,
  DiffItem,
  DiffStatus,
  ErrorInfo,
  Fidelity,
  ObjectKind,
  ObjectRef,
  RestoreEvent,
  RestoreProgress,
  RestoreReport,
  RowIssue,
  SourceObject,
  SyncScript,
  TableDataDiff,
  TableProgress,
  TableReport,
  TransferEvent,
  TransferOptions,
  TransferPlan,
  TransferReport,
  TransferSpec,
} from "../../api";
import { qualifiedName } from "../names";

export { qualifiedName };

// --- containers -------------------------------------------------------------------------------

/** A container's display name: `oxbase_test.m8_src` for a schema, `m8_src` for a database. */
export function containerLabel(ref: ObjectRef | null | undefined): string {
  if (!ref) return "";
  if (ref.kind === "schema") return [ref.catalog, ref.name].filter(Boolean).join(".");
  return ref.name;
}

/** Whether a ref is a database or a schema (what transfer, sync and backup work on). */
export function isContainer(ref: ObjectRef): boolean {
  return ref.kind === "database" || ref.kind === "schema";
}

/** Two refs name the same object (kind, catalog, schema, name). */
export function sameRef(a: ObjectRef | null | undefined, b: ObjectRef | null | undefined): boolean {
  if (!a || !b) return false;
  return (
    a.kind === b.kind &&
    a.name === b.name &&
    (a.catalog ?? "") === (b.catalog ?? "") &&
    (a.schema ?? "") === (b.schema ?? "")
  );
}

/** A plain-text plural (`1 table`, `3 tables`). */
export function plural(n: number, one: string, many: string): string {
  return `${n.toLocaleString("en-US")} ${n === 1 ? one : many}`;
}

// --- fidelity ----------------------------------------------------------------------------------

export const FIDELITY_LABEL: Record<Fidelity, string> = {
  exact: "exact",
  widened: "widened",
  narrowed: "narrowed",
  unsupported: "unsupported",
};

/** The badge modifier of a fidelity (colour always comes with the word). */
export function fidelityTone(f: Fidelity): "success" | "info" | "warning" | "danger" {
  switch (f) {
    case "exact":
      return "success";
    case "widened":
      return "info";
    case "narrowed":
      return "warning";
    case "unsupported":
      return "danger";
  }
}

/** What a fidelity means, for the badge tooltip when the mapper gave no reason. */
export const FIDELITY_HINT: Record<Fidelity, string> = {
  exact: "Every value fits and reads back equal.",
  widened: "Every value fits; the target type is wider or stores a text form.",
  narrowed: "Some values may not fit or lose detail.",
  unsupported: "The target has no type for this column: skip it or pick a type.",
};

// --- structure sync ---------------------------------------------------------------------------

export const STATUS_LABEL: Record<DiffStatus, string> = {
  only_in_source: "Only in source",
  only_in_target: "Only in target",
  different: "Different",
  same: "Identical",
};

/** What applying an item does to the target. */
export const STATUS_ACTION: Record<DiffStatus, string> = {
  only_in_source: "create",
  only_in_target: "drop",
  different: "alter",
  same: "none",
};

const KIND_GROUP: Partial<Record<ObjectKind, string>> = {
  table: "Tables",
  view: "Views",
  mat_view: "Materialized views",
  sequence: "Sequences",
  function: "Functions",
  procedure: "Procedures",
  trigger: "Triggers",
  index: "Indexes",
  foreign_key: "Foreign keys",
  check: "Checks",
  column: "Columns",
  type: "Types",
};

export function kindGroupLabel(kind: ObjectKind): string {
  return KIND_GROUP[kind] ?? kind.replace(/_/g, " ");
}

/** The kinds a structure compare looks at by default (docs/m8-contracts.md §5.1). */
export const SYNC_KIND_CHOICES: readonly ObjectKind[] = [
  "table",
  "view",
  "mat_view",
  "sequence",
  "function",
  "procedure",
  "trigger",
];

export interface ItemGroup {
  kind: ObjectKind;
  label: string;
  items: DiffItem[];
}

/** Items grouped by kind, in the core's order (kind rank, then name); `same` hidden unless asked. */
export function groupItems(items: DiffItem[], showSame: boolean): ItemGroup[] {
  const groups: ItemGroup[] = [];
  const byKind = new Map<ObjectKind, ItemGroup>();
  for (const item of items) {
    if (!showSame && item.status === "same") continue;
    let group = byKind.get(item.kind);
    if (!group) {
      group = { kind: item.kind, label: kindGroupLabel(item.kind), items: [] };
      byKind.set(item.kind, group);
      groups.push(group);
    }
    group.items.push(item);
  }
  return groups;
}

/** An item can be part of a sync (identical ones have nothing to do). */
export function selectable(item: DiffItem): boolean {
  return item.status !== "same";
}

/**
 * The items selected after a compare: what the source has and the target lacks or has
 * differently; target-only objects (drops) only when the compare included drops.
 */
export function defaultSelection(items: DiffItem[], includeDrops: boolean): Set<string> {
  const out = new Set<string>();
  for (const item of items) {
    if (item.status === "only_in_source" || item.status === "different") out.add(item.id);
    else if (item.status === "only_in_target" && includeDrops) out.add(item.id);
  }
  return out;
}

/** `checked`, `unchecked` or `mixed` for a group checkbox. */
export function groupState(group: ItemGroup, selection: ReadonlySet<string>): boolean | "mixed" {
  const ids = group.items.filter(selectable).map((i) => i.id);
  if (ids.length === 0) return false;
  const on = ids.filter((id) => selection.has(id)).length;
  if (on === 0) return false;
  return on === ids.length ? true : "mixed";
}

/** Selection with every selectable item of `group` turned on or off. */
export function setGroup(
  selection: ReadonlySet<string>,
  group: ItemGroup,
  on: boolean,
): Set<string> {
  const next = new Set(selection);
  for (const item of group.items) {
    if (!selectable(item)) continue;
    if (on) next.add(item.id);
    else next.delete(item.id);
  }
  return next;
}

/** Selection with one item toggled. */
export function toggle(selection: ReadonlySet<string>, id: string): Set<string> {
  const next = new Set(selection);
  if (next.has(id)) next.delete(id);
  else next.add(id);
  return next;
}

/** Selected ids in the items' order (what `structure_script` and `structure_apply` get). */
export function orderedSelection(items: DiffItem[], selection: ReadonlySet<string>): string[] {
  return items.filter((i) => selectable(i) && selection.has(i.id)).map((i) => i.id);
}

/** The destructive steps of a sync script, as people read them. */
export function destructiveSteps(script: SyncScript): string[] {
  return script.script.steps.filter((s) => s.destructive).map((s) => s.description ?? s.sql);
}

/** A property value as text (`null`, numbers, JSON). */
export function valueText(value: unknown): string {
  if (value === undefined) return "";
  if (value === null) return "NULL";
  if (typeof value === "object" && value !== null && "type" in value) {
    const v = value as { type: string; value?: unknown };
    if (v.type === "null") return "NULL";
    if (v.value === undefined) return v.type;
    if (typeof v.value === "string") return v.value;
    return JSON.stringify(v.value);
  }
  return typeof value === "string" ? value : JSON.stringify(value);
}

// --- transfer --------------------------------------------------------------------------------

/** One object of the wizard's object list. */
export interface TransferItem {
  /** The source table or view; absent for a query. */
  ref?: ObjectRef;
  /** The query of a query source. */
  sql?: string;
  targetName: string;
  /** A row filter (SQL condition) on a table or view; empty: every row. */
  where: string;
}

/** A column override typed in the mapping step, by object index and source column. */
export interface OverrideEdit {
  skip?: boolean;
  targetName?: string;
  /** `typeKey` of the chosen logical type. */
  dataType?: ColumnOverride["dataType"];
  nativeType?: string;
}

export type OverrideMap = Record<string, OverrideEdit>;

export function overrideKey(object: number, column: string): string {
  return `${object}\u0000${column}`;
}

/** The overrides as the spec carries them (empty edits left out). */
export function overridesOf(map: OverrideMap): ColumnOverride[] {
  const out: ColumnOverride[] = [];
  for (const [key, edit] of Object.entries(map)) {
    const cut = key.indexOf("\u0000");
    const object = Number(key.slice(0, cut));
    const column = key.slice(cut + 1);
    const o: ColumnOverride = { object, column };
    if (edit.skip) o.skip = true;
    if (edit.targetName?.trim()) o.targetName = edit.targetName.trim();
    if (edit.dataType) o.dataType = edit.dataType;
    if (edit.nativeType?.trim()) o.nativeType = edit.nativeType.trim();
    if (Object.keys(o).length > 2) out.push(o);
  }
  out.sort((a, b) => a.object - b.object || a.column.localeCompare(b.column));
  return out;
}

/** The source objects of a spec, from the wizard's list. */
export function sourceObjects(items: TransferItem[]): SourceObject[] {
  return items.map((item): SourceObject => {
    const targetName = item.targetName.trim();
    const where = item.where.trim();
    if (!item.ref) return { type: "query", sql: item.sql ?? "", targetName };
    const extra = {
      ...(targetName && targetName !== item.ref.name ? { targetName } : {}),
      ...(where ? { filter: { type: "rawSql" as const, sql: where } } : {}),
    };
    return item.ref.kind === "view"
      ? { type: "view", view: item.ref, ...extra }
      : { type: "table", table: item.ref, ...extra };
  });
}

export function buildSpec(
  items: TransferItem[],
  target: ObjectRef,
  options: TransferOptions,
  overrides: OverrideMap,
): TransferSpec {
  const o = overridesOf(overrides);
  return {
    objects: sourceObjects(items),
    target,
    options,
    ...(o.length > 0 ? { overrides: o } : {}),
  };
}

/** A problem that keeps the objects step from going on, or null. */
export function itemsProblem(items: TransferItem[]): string | null {
  if (items.length === 0) return "Choose at least one table, view or query.";
  const names = new Map<string, number>();
  for (const item of items) {
    const name = item.targetName.trim();
    if (!name) return "Every object needs a target name.";
    if (!item.ref && !(item.sql ?? "").trim()) return "The query is empty.";
    const key = name.toLowerCase();
    names.set(key, (names.get(key) ?? 0) + 1);
  }
  const twice = [...names.entries()].find(([, n]) => n > 1);
  return twice ? `Two objects would land in the same target table "${twice[0]}".` : null;
}

export const DEFAULT_TRANSFER_OPTIONS: Required<TransferOptions> = {
  mode: "create",
  batchRows: 10_000,
  transaction: "per_table",
  errors: "stop",
  identity: "keep",
  fkChecks: "keep",
  createIndexes: true,
  createForeignKeys: true,
  maxReportedErrors: 1000,
};

/** Everything `transfer_plan` reports that keeps a run from starting. */
export function planBlockers(plan: TransferPlan): string[] {
  const out = [...(plan.blocked ?? [])];
  if (!plan.runnable && out.length === 0) out.push("The plan cannot run as it is.");
  return out;
}

/** The whole dry-run DDL as one script: tables in load order, then the foreign keys. */
export function planDdl(plan: TransferPlan): string {
  const lines: string[] = [];
  const order = plan.order.length > 0 ? plan.order : plan.tables.map((_, i) => i);
  for (const index of order) {
    const table = plan.tables[index];
    if (!table) continue;
    lines.push(
      `-- ${qualifiedName(table.target)} (${table.mode}${table.exists ? ", exists" : ""})`,
    );
    for (const ddl of table.ddl ?? []) lines.push(ddl.trimEnd(), "");
    if ((table.ddl ?? []).length === 0) lines.push("-- no DDL", "");
  }
  const fks = plan.tables.flatMap((t) => t.foreignKeys ?? []);
  if (fks.length > 0) {
    lines.push("-- foreign keys, after the load");
    for (const fk of fks) lines.push(fk.trimEnd(), "");
  }
  return lines.join("\n").trimEnd();
}

export type RunStatus = "idle" | "starting" | "running" | "finished" | "failed" | "cancelled";

export interface TransferRunState {
  status: RunStatus;
  id: string | null;
  plan: TransferPlan | null;
  /** Latest progress per table index. */
  progress: Record<number, TableProgress>;
  /** Finished tables by index. */
  reports: Record<number, TableReport>;
  current: number | null;
  /** Row issues as they arrived (bounded). */
  issues: { table: number; issue: RowIssue }[];
  report: TransferReport | null;
  error: ErrorInfo | null;
  cancelling: boolean;
}

export const IDLE_TRANSFER: TransferRunState = {
  status: "idle",
  id: null,
  plan: null,
  progress: {},
  reports: {},
  current: null,
  issues: [],
  report: null,
  error: null,
  cancelling: false,
};

/** Row issues kept for the progress view (the report holds the rest). */
export const MAX_SHOWN_ISSUES = 200;

export type TransferAction =
  | { type: "start" }
  | { type: "event"; event: TransferEvent }
  | { type: "startFailed"; error: ErrorInfo }
  | { type: "cancelling" };

export function transferReducer(state: TransferRunState, action: TransferAction): TransferRunState {
  switch (action.type) {
    case "start":
      return { ...IDLE_TRANSFER, status: "starting" };
    case "startFailed":
      return { ...state, status: "failed", error: action.error };
    case "cancelling":
      return { ...state, cancelling: true };
    case "event":
      break;
  }
  const event = action.event;
  if (state.status === "finished" || state.status === "failed" || state.status === "cancelled") {
    return state;
  }
  switch (event.type) {
    case "started":
      return { ...state, status: "running", id: event.transferId, plan: event.plan };
    case "tableStarted":
      return { ...state, status: "running", current: event.table };
    case "progress":
      return {
        ...state,
        status: "running",
        progress: { ...state.progress, [event.progress.table]: event.progress },
      };
    case "rowError":
      return state.issues.length >= MAX_SHOWN_ISSUES
        ? state
        : { ...state, issues: [...state.issues, { table: event.table, issue: event.issue }] };
    case "tableFinished":
      return {
        ...state,
        reports: { ...state.reports, [event.report.table]: event.report },
        progress: { ...state.progress, [event.report.table]: event.report.progress },
        current: state.current === event.report.table ? null : state.current,
      };
    case "finished":
      return withReport({ ...state, status: "finished", current: null }, event.report);
    case "failed":
      return withReport(
        { ...state, status: "failed", error: event.error, current: null },
        event.report,
      );
    case "cancelled":
      return withReport({ ...state, status: "cancelled", current: null }, event.report);
  }
}

function withReport(state: TransferRunState, report: TransferReport): TransferRunState {
  const reports = { ...state.reports };
  const progress = { ...state.progress };
  for (const t of report.tables) {
    reports[t.table] = t;
    progress[t.table] = t.progress;
  }
  return { ...state, report, reports, progress, cancelling: false };
}

/** Totals of a run so far (all tables). */
export function transferTotals(state: TransferRunState): {
  rowsWritten: number;
  rowsTotal: number | null;
  bytes: number;
  errors: number;
  tablesDone: number;
} {
  let rowsWritten = 0;
  let bytes = 0;
  let errors = 0;
  let rowsTotal: number | null = 0;
  const plan = state.plan;
  const count = plan?.tables.length ?? 0;
  for (let i = 0; i < count; i += 1) {
    const p = state.progress[i];
    rowsWritten += p?.rowsWritten ?? 0;
    bytes += p?.bytes ?? 0;
    errors += p?.errors ?? 0;
    const total = p?.totalRows ?? plan?.tables[i]?.estimatedRows;
    if (total === undefined || rowsTotal === null) rowsTotal = null;
    else rowsTotal += total;
  }
  const tablesDone = Object.values(state.reports).filter((r) => r.status !== "not_run").length;
  return { rowsWritten, rowsTotal: count === 0 ? null : rowsTotal, bytes, errors, tablesDone };
}

// --- data sync -------------------------------------------------------------------------------

/** A table pair of the data sync view. */
export interface PairRow {
  source: ObjectRef;
  /** `null`: no table of that name in the target (cannot be compared). */
  target: ObjectRef | null;
  include: boolean;
  /** Key columns typed by the user (comma separated); empty: the table's own key. */
  keyColumns: string;
}

/**
 * Pairs every source table with the target table of the same name (exact first, then ignoring
 * case); tables without a partner are listed unchecked.
 */
export function autoPairs(sources: ObjectRef[], targets: ObjectRef[]): PairRow[] {
  const exact = new Map(targets.map((t) => [t.name, t]));
  const folded = new Map<string, ObjectRef>();
  for (const t of targets)
    if (!folded.has(t.name.toLowerCase())) folded.set(t.name.toLowerCase(), t);
  return sources.map((source) => {
    const target = exact.get(source.name) ?? folded.get(source.name.toLowerCase()) ?? null;
    return { source, target, include: target !== null, keyColumns: "" };
  });
}

/** The key columns typed for a pair, or `null` (the table's own key). */
export function parseKeyColumns(text: string): string[] | null {
  const columns = text
    .split(",")
    .map((c) => c.trim())
    .filter(Boolean);
  return columns.length > 0 ? columns : null;
}

export interface DataCompareState {
  status: RunStatus;
  id: string | null;
  compareId: string | null;
  tables: number;
  progress: Record<number, CompareProgress>;
  /** Finished tables by pair index. */
  results: Record<number, TableDataDiff>;
  result: DataCompareResult | null;
  error: ErrorInfo | null;
  cancelling: boolean;
}

export const IDLE_DATA_COMPARE: DataCompareState = {
  status: "idle",
  id: null,
  compareId: null,
  tables: 0,
  progress: {},
  results: {},
  result: null,
  error: null,
  cancelling: false,
};

export type DataCompareAction =
  | { type: "start" }
  | { type: "adopt"; result: DataCompareResult }
  | { type: "started"; id: string }
  | { type: "event"; event: DataCompareEvent }
  | { type: "startFailed"; error: ErrorInfo }
  | { type: "cancelling" };

export function dataCompareReducer(
  state: DataCompareState,
  action: DataCompareAction,
): DataCompareState {
  switch (action.type) {
    case "start":
      return { ...IDLE_DATA_COMPARE, status: "starting" };
    case "adopt": {
      const results: Record<number, TableDataDiff> = {};
      for (const t of action.result.tables) results[t.pair] = t;
      return {
        ...IDLE_DATA_COMPARE,
        status: "finished",
        compareId: action.result.compareId,
        tables: action.result.tables.length,
        results,
        result: action.result,
      };
    }
    case "started":
      return { ...state, id: action.id };
    case "startFailed":
      return { ...state, status: "failed", error: action.error };
    case "cancelling":
      return { ...state, cancelling: true };
    case "event":
      break;
  }
  const event = action.event;
  if (state.status === "finished" || state.status === "failed" || state.status === "cancelled") {
    return state;
  }
  switch (event.type) {
    case "started":
      return { ...state, status: "running", compareId: event.compareId, tables: event.tables };
    case "progress":
      return {
        ...state,
        status: "running",
        progress: { ...state.progress, [event.progress.pair]: event.progress },
      };
    case "tableFinished":
      return { ...state, results: { ...state.results, [event.table.pair]: event.table } };
    case "finished":
    case "failed":
    case "cancelled": {
      const results = { ...state.results };
      for (const t of event.result.tables) results[t.pair] = t;
      return {
        ...state,
        status: event.type,
        compareId: event.result.compareId || state.compareId,
        result: event.result,
        results,
        error: event.type === "failed" ? event.error : null,
        cancelling: false,
      };
    }
  }
}

/** Sums of the compared tables' differences. */
export function dataTotals(results: TableDataDiff[]): {
  inserts: number;
  updates: number;
  deletes: number;
} {
  let inserts = 0;
  let updates = 0;
  let deletes = 0;
  for (const t of results) {
    if (t.status.type !== "compared") continue;
    inserts += t.counts.inserts;
    updates += t.counts.updates;
    deletes += t.counts.deletes;
  }
  return { inserts, updates, deletes };
}

export function compareStatusText(t: TableDataDiff): string {
  switch (t.status.type) {
    case "compared": {
      const c = t.counts;
      return c.inserts + c.updates + c.deletes === 0 ? "In sync" : "Different";
    }
    case "refused":
      return `Refused: ${t.status.reason}`;
    case "failed":
      return `Failed: ${t.status.message}`;
    case "cancelled":
      return "Cancelled";
  }
}

export interface DataApplyState {
  status: RunStatus | "recomparing";
  id: string | null;
  done: number;
  total: number;
  report: DataApplyReport | null;
  error: ErrorInfo | null;
  cancelling: boolean;
}

export const IDLE_DATA_APPLY: DataApplyState = {
  status: "idle",
  id: null,
  done: 0,
  total: 0,
  report: null,
  error: null,
  cancelling: false,
};

export type DataApplyAction =
  | { type: "start" }
  | { type: "reset" }
  | { type: "started"; id: string }
  | { type: "event"; event: DataApplyEvent }
  | { type: "startFailed"; error: ErrorInfo }
  | { type: "cancelling" };

export function dataApplyReducer(state: DataApplyState, action: DataApplyAction): DataApplyState {
  switch (action.type) {
    case "start":
      return { ...IDLE_DATA_APPLY, status: "starting" };
    case "reset":
      return IDLE_DATA_APPLY;
    case "started":
      return { ...state, id: action.id };
    case "startFailed":
      return { ...state, status: "failed", error: action.error };
    case "cancelling":
      return { ...state, cancelling: true };
    case "event":
      break;
  }
  const event = action.event;
  if (state.status === "finished" || state.status === "failed" || state.status === "cancelled") {
    return state;
  }
  switch (event.type) {
    case "started":
      return { ...state, status: "running", id: event.applyId, total: event.totalStatements };
    case "progress":
      return {
        ...state,
        status: "running",
        done: event.statementsDone,
        total: event.totalStatements,
      };
    case "recomparing":
      return { ...state, status: "recomparing" };
    case "finished":
    case "cancelled":
      return { ...state, status: event.type, report: event.report, cancelling: false };
    case "failed":
      return {
        ...state,
        status: "failed",
        report: event.report,
        error: event.error,
        cancelling: false,
      };
  }
}

// --- backup / restore ------------------------------------------------------------------------

export interface BackupRunState {
  status: RunStatus;
  id: string | null;
  objects: number;
  progress: BackupProgress | null;
  report: BackupReport | null;
  error: ErrorInfo | null;
  elapsedMs: number;
  cancelling: boolean;
}

export const IDLE_BACKUP: BackupRunState = {
  status: "idle",
  id: null,
  objects: 0,
  progress: null,
  report: null,
  error: null,
  elapsedMs: 0,
  cancelling: false,
};

export type BackupAction =
  | { type: "start" }
  | { type: "started"; id: string }
  | { type: "event"; event: BackupEvent }
  | { type: "startFailed"; error: ErrorInfo }
  | { type: "cancelling" };

export function backupReducer(state: BackupRunState, action: BackupAction): BackupRunState {
  switch (action.type) {
    case "start":
      return { ...IDLE_BACKUP, status: "starting" };
    case "started":
      return { ...state, id: action.id };
    case "startFailed":
      return { ...state, status: "failed", error: action.error };
    case "cancelling":
      return { ...state, cancelling: true };
    case "event":
      break;
  }
  const event = action.event;
  if (state.status === "finished" || state.status === "failed" || state.status === "cancelled") {
    return state;
  }
  switch (event.type) {
    case "started":
      return { ...state, status: "running", id: event.backupId, objects: event.objects };
    case "progress":
      return {
        ...state,
        status: "running",
        progress: event.progress,
        elapsedMs: event.progress.elapsedMs,
      };
    case "finished":
      return {
        ...state,
        status: "finished",
        report: event.report,
        elapsedMs: event.report.elapsedMs,
        cancelling: false,
      };
    case "failed":
      return {
        ...state,
        status: "failed",
        error: event.error,
        elapsedMs: event.elapsedMs,
        cancelling: false,
      };
    case "cancelled":
      return { ...state, status: "cancelled", elapsedMs: event.elapsedMs, cancelling: false };
  }
}

export interface RestoreRunState {
  status: RunStatus;
  id: string | null;
  progress: RestoreProgress | null;
  report: RestoreReport | null;
  error: ErrorInfo | null;
  cancelling: boolean;
}

export const IDLE_RESTORE: RestoreRunState = {
  status: "idle",
  id: null,
  progress: null,
  report: null,
  error: null,
  cancelling: false,
};

export type RestoreAction =
  | { type: "start" }
  | { type: "started"; id: string }
  | { type: "event"; event: RestoreEvent }
  | { type: "startFailed"; error: ErrorInfo }
  | { type: "cancelling" };

export function restoreReducer(state: RestoreRunState, action: RestoreAction): RestoreRunState {
  switch (action.type) {
    case "start":
      return { ...IDLE_RESTORE, status: "starting" };
    case "started":
      return { ...state, id: action.id };
    case "startFailed":
      return { ...state, status: "failed", error: action.error };
    case "cancelling":
      return { ...state, cancelling: true };
    case "event":
      break;
  }
  const event = action.event;
  if (state.status === "finished" || state.status === "failed" || state.status === "cancelled") {
    return state;
  }
  switch (event.type) {
    case "started":
      return { ...state, status: "running", id: event.restoreId };
    case "progress":
      return { ...state, status: "running", progress: event.progress };
    case "finished":
    case "cancelled":
      return { ...state, status: event.type, report: event.report, cancelling: false };
    case "failed":
      return {
        ...state,
        status: "failed",
        report: event.report,
        error: event.error,
        cancelling: false,
      };
  }
}

/** Whether a run has ended (any terminal state). */
export function ended(status: string): boolean {
  return status === "finished" || status === "failed" || status === "cancelled";
}

/** A safe file name for a backup of `container` (`oxbase_test-m8_src.oxbackup`). */
export function backupFileName(container: ObjectRef | null, now = new Date()): string {
  const base = (containerLabel(container) || "backup").replace(/[^A-Za-z0-9_.-]+/g, "_");
  const p = (n: number) => n.toString().padStart(2, "0");
  const stamp = `${now.getFullYear()}${p(now.getMonth() + 1)}${p(now.getDate())}-${p(now.getHours())}${p(now.getMinutes())}`;
  return `${base.replace(/\./g, "-")}-${stamp}.oxbackup`;
}
