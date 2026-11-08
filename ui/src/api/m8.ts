/**
 * M8 data transfer, structure sync, data sync, backup and restore: payload types (mirrors of
 * `oxbase_transfer`, `oxbase_diff`, `oxbase_backup` and `oxbase_services::{transfer, sync,
 * backup, runs}`) and typed wrappers of the commands in `crates/app-tauri/src/sync.rs`
 * (docs/m8-contracts.md §8).
 *
 * Naming: wrapper payloads are camelCase; core models inside (`ObjectRef`, `DataType`,
 * `Filter`, `PropertyDiff`, `Script`) keep their snake_case serde form; enum values are
 * snake_case strings (`only_in_source`, `per_table`), event `type`s camelCase (`tableStarted`).
 *
 * Backup files are opaque handles from `filePickSave` / `filePickOpen` (with `BACKUP_FILTERS`);
 * this module never sends a file-system path.
 */
import { Channel } from "@tauri-apps/api/core";
import { call } from "./call";
import type { FileFilter, FileHandle } from "./io";
import type {
  DataType,
  Engine,
  ErrorInfo,
  Filter,
  ObjectKind,
  ObjectRef,
  PropertyDiff,
  Script,
  ScriptOutcome,
} from "./types";

/** `oxbase_services::runs::OperationId`: a transfer, compare, apply, backup or restore. */
export type OperationId = string;

// --- type mapping (oxbase_dialect::typemap) ---------------------------------------------------

export type Fidelity = "exact" | "widened" | "narrowed" | "unsupported";

/** Best to worst; `worstFidelity` combines. */
export const FIDELITY_ORDER: readonly Fidelity[] = ["exact", "widened", "narrowed", "unsupported"];

export function worstFidelity(...items: Fidelity[]): Fidelity {
  let worst: Fidelity = "exact";
  for (const f of items) {
    if (FIDELITY_ORDER.indexOf(f) > FIDELITY_ORDER.indexOf(worst)) worst = f;
  }
  return worst;
}

export interface TypeMapping {
  dataType: DataType;
  /** Empty / absent: the target's DDL generator derives it from `dataType`. */
  nativeType?: string;
  fidelity: Fidelity;
  /** Why the mapping is not exact (the badge tooltip). */
  reason?: string;
}

// --- transfer (oxbase_transfer, oxbase_services::transfer) ------------------------------------

export type SourceObject =
  | { type: "table"; table: ObjectRef; targetName?: string; filter?: Filter }
  | { type: "view"; view: ObjectRef; targetName?: string; filter?: Filter }
  | { type: "query"; sql: string; targetName: string };

export type TableMode = "create" | "replace" | "append" | "truncate";
export type TransactionScope = "per_table" | "per_batch";
export type ErrorMode = "stop" | "continue";
export type IdentityMode = "keep" | "generate";
export type FkChecks = "keep" | "disable";

/** Every field optional: the core fills its defaults. */
export interface TransferOptions {
  mode?: TableMode;
  batchRows?: number;
  transaction?: TransactionScope;
  errors?: ErrorMode;
  identity?: IdentityMode;
  fkChecks?: FkChecks;
  createIndexes?: boolean;
  createForeignKeys?: boolean;
  maxReportedErrors?: number;
}

export interface ColumnOverride {
  /** Index into `TransferSpec.objects`. */
  object: number;
  column: string;
  skip?: boolean;
  targetName?: string;
  dataType?: DataType;
  nativeType?: string;
}

export interface TransferSpec {
  objects: SourceObject[];
  /** A `schema` ref (PostgreSQL, SQL Server) or a `database` ref (MySQL, MariaDB, SQLite). */
  target: ObjectRef;
  options?: TransferOptions;
  overrides?: ColumnOverride[];
}

export interface TransferRequest {
  sourceSession: string;
  targetSession: string;
  spec: TransferSpec;
  confirmDestructive?: boolean;
}

export interface ColumnPlan {
  source: string;
  sourceType: DataType;
  sourceNative?: string;
  target: string;
  mapping: TypeMapping;
  overridden: boolean;
  skipped: boolean;
}

export interface TablePlan {
  object: number;
  source?: ObjectRef;
  target: ObjectRef;
  exists: boolean;
  mode: TableMode;
  columns: ColumnPlan[];
  fidelity: Fidelity;
  ddl?: string[];
  foreignKeys?: string[];
  estimatedRows?: number;
  warnings?: string[];
}

/** `transfer_plan` (also the dry run). */
export interface TransferPlan {
  sourceEngine: Engine;
  targetEngine: Engine;
  tables: TablePlan[];
  /** Load order: indexes into `tables`. */
  order: number[];
  cycles?: number[][];
  fidelity: Fidelity;
  destructive: boolean;
  transactionalDdl: boolean;
  runnable: boolean;
  blocked?: string[];
  warnings?: string[];
}

export interface TableProgress {
  table: number;
  rowsRead: number;
  rowsWritten: number;
  rowsCommitted: number;
  rowsSkipped: number;
  bytes: number;
  errors: number;
  totalRows?: number;
  elapsedMs: number;
}

export interface RowIssue {
  row: number;
  column?: string;
  message: string;
}

export type TableStatus = "done" | "failed" | "cancelled" | "not_run";

export interface TableReport {
  table: number;
  target: ObjectRef;
  status: TableStatus;
  progress: TableProgress;
  issues?: RowIssue[];
  issuesTruncated: boolean;
  error?: string;
  leftBehind: string;
}

export interface TransferReport {
  tables: TableReport[];
  foreignKeysAdded: number;
  warnings?: string[];
  elapsedMs: number;
}

export type TransferEvent =
  | { type: "started"; transferId: OperationId; plan: TransferPlan }
  | { type: "tableStarted"; table: number }
  | { type: "progress"; progress: TableProgress }
  | { type: "rowError"; table: number; issue: RowIssue }
  | { type: "tableFinished"; report: TableReport }
  | { type: "finished"; report: TransferReport }
  | { type: "failed"; error: ErrorInfo; report: TransferReport }
  | { type: "cancelled"; report: TransferReport };

export interface PairRow {
  logical: DataType;
  mapping: TypeMapping;
}

/** `transfer_pair_report`: how each representative logical type maps between two engines. */
export interface PairReport {
  source: Engine;
  target: Engine;
  rows: PairRow[];
}

// --- structure sync (oxbase_diff, oxbase_services::sync) --------------------------------------

/** Every field optional: the core fills its defaults (no drops, comments and collations compared). */
export interface StructureOptions {
  kinds?: ObjectKind[];
  includeDrops?: boolean;
  compareCollations?: boolean;
  compareComments?: boolean;
  compareOwnership?: boolean;
  ignoreCase?: boolean;
  exclude?: ObjectRef[];
}

export type DiffStatus = "only_in_source" | "only_in_target" | "different" | "same";

export interface DiffItem {
  /** Stable across compares of the same schemas; what selections name. */
  id: string;
  kind: ObjectKind;
  name: string;
  parent?: string;
  status: DiffStatus;
  source?: ObjectRef;
  target?: ObjectRef;
  properties?: PropertyDiff[];
  sourceDdl?: string;
  targetDdl?: string;
}

export interface StructureDiff {
  engine: Engine;
  source: ObjectRef;
  target: ObjectRef;
  items: DiffItem[];
  warnings?: string[];
}

export interface DiffCounts {
  onlyInSource: number;
  onlyInTarget: number;
  different: number;
  same: number;
}

export interface SkippedObject {
  object: ObjectRef;
  reason: string;
}

export interface StructureCompareRequest {
  sourceSession: string;
  source: ObjectRef;
  targetSession: string;
  target: ObjectRef;
  options?: StructureOptions;
}

export interface StructureCompareResult {
  compareId: OperationId;
  diff: StructureDiff;
  counts: DiffCounts;
  skipped?: SkippedObject[];
  elapsedMs: number;
}

export interface StructureScriptRequest {
  compareId: OperationId;
  /** Item ids; empty: every differing item. */
  selection?: string[];
}

export interface SyncScript {
  engine: Engine;
  script: Script;
  /** The whole script as text (Monaco preview, save). */
  sql: string;
  items: string[];
  destructive: boolean;
  atomic: boolean;
  transactionalDdl: boolean;
  warnings?: string[];
}

export interface StructureApplyRequest {
  compareId: OperationId;
  selection?: string[];
  confirmDestructive?: boolean;
}

export interface StructureApplyReport {
  script: SyncScript;
  outcome: ScriptOutcome;
  recompare: StructureCompareResult;
  inSync: boolean;
}

// --- data sync --------------------------------------------------------------------------------

export type KeyChoice =
  | { type: "primary" }
  | { type: "unique"; name: string }
  | { type: "columns"; columns: string[] };

export interface TablePair {
  source: ObjectRef;
  target: ObjectRef;
  key?: KeyChoice;
  columns?: string[];
}

export interface DataCompareOptions {
  chunkRows?: number;
  sampleRows?: number;
  useChunkHash?: boolean;
}

export interface DataCompareRequest {
  sourceSession: string;
  targetSession: string;
  pairs: TablePair[];
  options?: DataCompareOptions;
}

export type RowOp = "insert" | "update" | "delete";

export interface RowCounts {
  sourceRows: number;
  targetRows: number;
  same: number;
  inserts: number;
  updates: number;
  deletes: number;
}

/** Cells as display text; `null` is SQL NULL. */
export interface RowSample {
  op: RowOp;
  key: (string | null)[];
  source?: (string | null)[];
  target?: (string | null)[];
  changed?: string[];
}

export type TableCompareStatus =
  | { type: "compared" }
  | { type: "refused"; reason: string }
  | { type: "failed"; message: string }
  | { type: "cancelled" };

export interface TableDataDiff {
  pair: number;
  source: ObjectRef;
  target: ObjectRef;
  status: TableCompareStatus;
  key: string[];
  columns: string[];
  counts: RowCounts;
  samples?: RowSample[];
  chunksSkipped: number;
  warnings?: string[];
  elapsedMs: number;
}

export interface CompareProgress {
  pair: number;
  counts: RowCounts;
  chunks: number;
  chunksSkipped: number;
  elapsedMs: number;
}

export interface DataCompareResult {
  compareId: OperationId;
  tables: TableDataDiff[];
  elapsedMs: number;
}

export type DataCompareEvent =
  | { type: "started"; compareId: OperationId; tables: number }
  | { type: "progress"; progress: CompareProgress }
  | { type: "tableFinished"; table: TableDataDiff }
  | { type: "finished"; result: DataCompareResult }
  | { type: "failed"; error: ErrorInfo; result: DataCompareResult }
  | { type: "cancelled"; result: DataCompareResult };

/** Deletes are off unless asked (destructive). */
export interface DataScriptOptions {
  inserts?: boolean;
  updates?: boolean;
  deletes?: boolean;
  pairs?: number[];
  previewLimit?: number;
}

export interface DataScriptRequest {
  compareId: OperationId;
  options?: DataScriptOptions;
}

export interface DataScript {
  engine: Engine;
  statements: string[];
  totalStatements: number;
  truncated: boolean;
  destructive: boolean;
  warnings?: string[];
}

export interface DataApplyOptions {
  script?: DataScriptOptions;
  batchStatements?: number;
  confirmDeletes?: boolean;
}

export interface DataApplyRequest {
  compareId: OperationId;
  options?: DataApplyOptions;
}

export interface TableApplyReport {
  pair: number;
  target: ObjectRef;
  inserted: number;
  updated: number;
  deleted: number;
  error?: string;
}

export interface DataApplyReport {
  tables: TableApplyReport[];
  statements: number;
  committedBatches: number;
  leftBehind: string;
  recompare?: DataCompareResult;
  inSync: boolean;
  elapsedMs: number;
}

export type DataApplyEvent =
  | { type: "started"; applyId: OperationId; totalStatements: number }
  | {
      type: "progress";
      statementsDone: number;
      totalStatements: number;
      pair: number;
      elapsedMs: number;
    }
  | { type: "recomparing" }
  | { type: "finished"; report: DataApplyReport }
  | { type: "failed"; error: ErrorInfo; report: DataApplyReport }
  | { type: "cancelled"; report: DataApplyReport };

// --- backup / restore (oxbase_backup, oxbase_services::backup) --------------------------------

/** `data_only`: the tables' rows only (their definitions are kept just to read them back). */
export type BackupContent = "structure_and_data" | "structure_only" | "data_only";
export type Compression = { codec: "none" } | { codec: "zstd" };

export interface BackupSpec {
  container: ObjectRef;
  /** Empty: every object of the container. */
  objects?: ObjectRef[];
  content?: BackupContent;
  compression?: Compression;
}

export interface BackupRequest {
  session: string;
  spec: BackupSpec;
}

export interface BackupProgress {
  objectsDone: number;
  objectsTotal: number;
  current?: ObjectRef;
  rows: number;
  bytes: number;
  elapsedMs: number;
}

export interface TableData {
  entry: string;
  rows: number;
  columns: string[];
}

export interface ManifestObject {
  index: number;
  kind: ObjectKind;
  ref: ObjectRef;
  definition: string;
  ddl?: string;
  data?: TableData;
}

export interface EntryInfo {
  size: number;
  sha256: string;
}

/** `backup_list`: the archive's table of contents (never connection details). */
export interface Manifest {
  format: string;
  formatVersion: number;
  oxbaseVersion: string;
  createdAt: string;
  engine: Engine;
  serverVersion: string;
  source: { container: ObjectRef };
  content: BackupContent;
  compression: Compression;
  objects: ManifestObject[];
  order: number[];
  entries: Record<string, EntryInfo>;
  warnings?: string[];
}

export interface BackupReport {
  manifest: Manifest;
  bytes: number;
  elapsedMs: number;
}

export type BackupEvent =
  | { type: "started"; backupId: OperationId; objects: number }
  | { type: "progress"; progress: BackupProgress }
  | { type: "finished"; report: BackupReport }
  | { type: "failed"; error: ErrorInfo; elapsedMs: number }
  | { type: "cancelled"; elapsedMs: number };

export type RestoreContent = "structure_and_data" | "structure_only" | "data_only";

export interface RestoreOptions {
  /** An existing database or schema. */
  container: ObjectRef;
  content?: RestoreContent;
  /** Manifest object indexes; empty: all. */
  objects?: number[];
  dropExisting?: boolean;
  truncateBeforeLoad?: boolean;
  verifyFirst?: boolean;
  confirmDestructive?: boolean;
  batchRows?: number;
}

export interface RestoreRequest {
  session: string;
  options: RestoreOptions;
}

export interface RestoreProgress {
  /** `verify`, `structure` or `data`. */
  phase: string;
  objectsDone: number;
  objectsTotal: number;
  current?: ObjectRef;
  rows: number;
  rowsTotal: number;
  elapsedMs: number;
}

export type RestoredStatus = "created" | "replaced" | "kept" | "loaded" | "failed" | "not_run";

export interface RestoredObject {
  index: number;
  object: ObjectRef;
  status: RestoredStatus;
  rows: number;
  error?: string;
}

export interface RestoreReport {
  objects: RestoredObject[];
  rows: number;
  leftBehind: string;
  warnings?: string[];
  elapsedMs: number;
}

export type RestoreEvent =
  | { type: "started"; restoreId: OperationId; manifest: Manifest }
  | { type: "progress"; progress: RestoreProgress }
  | { type: "finished"; report: RestoreReport }
  | { type: "failed"; error: ErrorInfo; report: RestoreReport }
  | { type: "cancelled"; report: RestoreReport };

export interface EntryCheck {
  entry: string;
  ok: boolean;
  problem?: string;
}

export interface VerifyReport {
  ok: boolean;
  entriesChecked: number;
  bytesChecked: number;
  problems?: EntryCheck[];
}

/** Dialog filters for backup archives (`.oxbackup`). */
export const BACKUP_FILTERS: FileFilter[] = [{ name: "Oxbase backup", extensions: ["oxbackup"] }];

/** `true` for the last event of any M8 run (`finished`, `failed`, `cancelled`). */
export function isTerminalM8Event(
  event: TransferEvent | DataCompareEvent | DataApplyEvent | BackupEvent | RestoreEvent,
): boolean {
  return event.type === "finished" || event.type === "failed" || event.type === "cancelled";
}

// --- commands ---------------------------------------------------------------------------------

function channelFor<T>(onEvent: (event: T) => void): Channel<T> {
  const channel = new Channel<T>();
  channel.onmessage = onEvent;
  return channel;
}

/** What `transferRun` would do (target DDL, mapping report, order). Changes nothing. */
export function transferPlan(request: TransferRequest, requestId?: string): Promise<TransferPlan> {
  return call("transfer_plan", { request, requestId: requestId ?? null });
}

/** How every representative logical type maps between the engines of two sessions. */
export function transferPairReport(
  sourceSession: string,
  targetSession: string,
): Promise<PairReport> {
  return call("transfer_pair_report", { sourceSession, targetSession });
}

/** Starts a transfer; events arrive on `onEvent`. */
export function transferRun(
  request: TransferRequest,
  onEvent: (event: TransferEvent) => void,
): Promise<OperationId> {
  return call("transfer_run", { request, onEvent: channelFor(onEvent) });
}

export function transferCancel(transferId: OperationId): Promise<boolean> {
  return call("transfer_cancel", { transferId });
}

export function structureCompare(
  request: StructureCompareRequest,
  requestId?: string,
): Promise<StructureCompareResult> {
  return call("structure_compare", { request, requestId: requestId ?? null });
}

/** The sync script of the selected items. Changes nothing. */
export function structureScript(request: StructureScriptRequest): Promise<SyncScript> {
  return call("structure_script", { request });
}

/** Applies the selected items and re-compares. */
export function structureApply(
  request: StructureApplyRequest,
  requestId?: string,
): Promise<StructureApplyReport> {
  return call("structure_apply", { request, requestId: requestId ?? null });
}

/** Drops a structure or data compare the core keeps (call when its view closes). */
export function compareForget(compareId: OperationId): Promise<boolean> {
  return call("compare_forget", { compareId });
}

/** Starts a data compare; the terminal event's result carries the `compareId`. */
export function dataCompare(
  request: DataCompareRequest,
  onEvent: (event: DataCompareEvent) => void,
): Promise<OperationId> {
  return call("data_compare", { request, onEvent: channelFor(onEvent) });
}

export function dataScript(request: DataScriptRequest, requestId?: string): Promise<DataScript> {
  return call("data_script", { request, requestId: requestId ?? null });
}

export function dataApply(
  request: DataApplyRequest,
  onEvent: (event: DataApplyEvent) => void,
): Promise<OperationId> {
  return call("data_apply", { request, onEvent: channelFor(onEvent) });
}

/** Cancels a running data compare or apply. */
export function dataCancel(operationId: OperationId): Promise<boolean> {
  return call("data_cancel", { operationId });
}

/** Starts a backup into a file picked with `filePickSave(…, BACKUP_FILTERS)`. */
export function backupRun(
  file: FileHandle,
  request: BackupRequest,
  onEvent: (event: BackupEvent) => void,
): Promise<OperationId> {
  return call("backup_run", { file, request, onEvent: channelFor(onEvent) });
}

/** Starts restoring an archive picked with `filePickOpen(BACKUP_FILTERS)`. */
export function backupRestore(
  file: FileHandle,
  request: RestoreRequest,
  onEvent: (event: RestoreEvent) => void,
): Promise<OperationId> {
  return call("backup_restore", { file, request, onEvent: channelFor(onEvent) });
}

export function backupVerify(file: FileHandle, requestId?: string): Promise<VerifyReport> {
  return call("backup_verify", { file, requestId: requestId ?? null });
}

export function backupList(file: FileHandle, requestId?: string): Promise<Manifest> {
  return call("backup_list", { file, requestId: requestId ?? null });
}

/** Cancels a running backup or restore. */
export function backupCancel(operationId: OperationId): Promise<boolean> {
  return call("backup_cancel", { operationId });
}
