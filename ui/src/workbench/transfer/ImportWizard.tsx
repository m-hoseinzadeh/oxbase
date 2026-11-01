/**
 * The import wizard (docs/m5-contracts.md §5): pick a file in the native dialog (shown by Rust;
 * the wizard only holds an opaque handle) → format options with a live preview of the first
 * rows and the inferred types → target (a new table, or an existing one: append, empty first,
 * replace) → column mapping with type overrides and skips → summary (the core's plan) → run
 * with progress and cancel → report with the error log.
 */
import { type ReactNode, useEffect, useId, useReducer, useRef, useState } from "react";
import {
  type ColumnDef,
  type ConversionOptions,
  type ErrorInfo,
  type ErrorPolicy,
  filePickOpen,
  fileRelease,
  type ImportEvent,
  type ImportPlan,
  type ImportReport,
  type ImportRowError,
  importCancel,
  importPlan,
  importPreview,
  importRun,
  isTerminalTransferEvent,
  type ObjectRef,
  objectLoad,
  type PickedFile,
  type ReadOptions,
  type SourcePreview,
  type TargetMode,
  type TransactionPolicy,
} from "../../api";
import { Modal } from "../../components/Modal";
import { cancelRequest, newRequestId, toCommandError } from "../../connections/api";
import { useConnections } from "../../connections/store";
import { ENGINE_INFO } from "../../connections/types";
import { formatMs } from "../format";
import { openTableData } from "../layout";
import {
  buildImportRequest,
  createsTable,
  defaultReadOptions,
  FORMAT_LABEL,
  formatBytes,
  formatCount,
  IMPORT_FORMATS,
  type ImportFormat,
  importFilters,
  initialMapping,
  type MappingProblem,
  type MappingRow,
  MODE_LABEL,
  mappingProblems,
  qualifiedName,
  rates,
  readOptionsFor,
  readOptionsProblem,
  tableNameFromFile,
  typeChoices,
  typeKey,
  typeLabel,
} from "./model";
import { ProgressView } from "./ProgressView";
import { ReadOptionsForm } from "./ReadOptionsForm";
import { type ImportInit, setTransferBusy } from "./store";

export type Step = "file" | "format" | "target" | "mapping" | "summary" | "run";

const STEPS: { id: Step; label: string }[] = [
  { id: "file", label: "File" },
  { id: "format", label: "Format" },
  { id: "target", label: "Target" },
  { id: "mapping", label: "Mapping" },
  { id: "summary", label: "Summary" },
  { id: "run", label: "Import" },
];

/** Wait after an option change before the preview is asked for again. */
const PREVIEW_DELAY_MS = 150;

type PreviewState =
  | { status: "idle" }
  | { status: "loading"; data: SourcePreview | null }
  | { status: "ok"; data: SourcePreview }
  | { status: "error"; message: string; data: SourcePreview | null };

interface TargetForm {
  sessionId: string;
  kind: "new" | "existing";
  /** Existing table: what happens to its rows first. */
  existingMode: Exclude<TargetMode, "create">;
  catalog: string;
  schema: string;
  name: string;
  /** The name was suggested from the file (a new file suggests again). */
  suggested: boolean;
}

type PlanState =
  | { status: "idle" }
  | { status: "loading" }
  | { status: "ok"; plan: ImportPlan }
  | { status: "error"; message: string };

// --- run state ------------------------------------------------------------------------------

type ProgressEvent = Extract<ImportEvent, { type: "progress" }>;

export interface ImportRunState {
  status: "idle" | "starting" | "running" | "finished" | "failed" | "cancelled";
  importId: string | null;
  totalBytes: number;
  progress: ProgressEvent | null;
  /** Row errors as they arrive (the core sends the first 100). */
  rowErrors: ImportRowError[];
  report: ImportReport | null;
  error: ErrorInfo | null;
  cancelling: boolean;
}

const IDLE_IMPORT: ImportRunState = {
  status: "idle",
  importId: null,
  totalBytes: 0,
  progress: null,
  rowErrors: [],
  report: null,
  error: null,
  cancelling: false,
};

type RunAction =
  | { type: "start" }
  | { type: "id"; importId: string }
  | { type: "event"; event: ImportEvent }
  | { type: "startFailed"; error: ErrorInfo }
  | { type: "cancelling" };

function applyEvent(state: ImportRunState, event: ImportEvent): ImportRunState {
  switch (event.type) {
    case "started":
      return {
        ...state,
        status: state.status === "starting" ? "running" : state.status,
        importId: event.importId,
        totalBytes: event.totalBytes,
      };
    case "progress":
      return { ...state, progress: event, totalBytes: event.totalBytes || state.totalBytes };
    case "rowError":
      return { ...state, rowErrors: [...state.rowErrors, event.error] };
    case "committed":
      return state.progress
        ? { ...state, progress: { ...state.progress, rowsCommitted: event.rowsCommitted } }
        : state;
    case "finished":
      return { ...state, status: "finished", report: event.report };
    case "failed":
      return { ...state, status: "failed", report: event.report, error: event.error };
    case "cancelled":
      return { ...state, status: "cancelled", report: event.report };
  }
}

export function importRunReducer(state: ImportRunState, action: RunAction): ImportRunState {
  switch (action.type) {
    case "start":
      return { ...IDLE_IMPORT, status: "starting" };
    case "id":
      return { ...state, importId: action.importId };
    case "startFailed":
      return { ...state, status: "failed", error: action.error };
    case "cancelling":
      return { ...state, cancelling: true };
    case "event":
      return applyEvent(state, action.event);
  }
}

// --- helpers --------------------------------------------------------------------------------

function initialTarget(init: ImportInit, fallbackSession: string): TargetForm {
  const base: TargetForm = {
    sessionId: init.sessionId ?? fallbackSession,
    kind: "new",
    existingMode: "append",
    catalog: "",
    schema: "",
    name: "",
    suggested: true,
  };
  const t = init.target;
  if (!t) return base;
  switch (t.kind) {
    case "table":
      return {
        ...base,
        kind: "existing",
        catalog: t.catalog ?? "",
        schema: t.schema ?? "",
        name: t.name,
        suggested: false,
      };
    case "schema":
      return { ...base, catalog: t.catalog ?? "", schema: t.name };
    case "database":
      return { ...base, catalog: t.name };
    default:
      return { ...base, catalog: t.catalog ?? "", schema: t.schema ?? "" };
  }
}

function targetRef(form: TargetForm): ObjectRef {
  const catalog = form.catalog.trim();
  const schema = form.schema.trim();
  return {
    kind: "table",
    ...(catalog ? { catalog } : {}),
    ...(schema ? { schema } : {}),
    name: form.name.trim(),
  };
}

function modeOf(form: TargetForm): TargetMode {
  return form.kind === "new" ? "create" : form.existingMode;
}

function formatOf(options: ReadOptions): ImportFormat {
  return options.format;
}

const METHOD_LABEL: Record<string, string> = {
  copy_text: "COPY (text)",
  copy_binary: "COPY (binary)",
  multi_row_insert: "Multi-row INSERT",
  prepared_insert: "Prepared INSERT",
  tds_bulk_load: "Bulk load (TDS)",
};

function PreviewGrid({ preview }: { preview: SourcePreview }) {
  return (
    <div className="transfer-preview" data-testid="preview-grid">
      <table className="transfer-table">
        <thead>
          <tr>
            {preview.columns.map((c) => (
              <th key={c.index} scope="col">
                <span className="transfer-table__name">{c.name}</span>
                <span className="transfer-table__type" data-testid="preview-type">
                  {typeLabel(c.dataType)}
                </span>
              </th>
            ))}
          </tr>
        </thead>
        <tbody>
          {preview.rows.map((row, r) => (
            // biome-ignore lint/suspicious/noArrayIndexKey: preview rows have no identity.
            <tr key={r}>
              {preview.columns.map((c) => {
                const value = row[c.index];
                return value === null || value === undefined ? (
                  <td key={c.index} className="transfer-table__null">
                    NULL
                  </td>
                ) : (
                  <td key={c.index}>{value}</td>
                );
              })}
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}

function previewSummary(preview: SourcePreview): string {
  const parts = [
    `${preview.columns.length} ${preview.columns.length === 1 ? "column" : "columns"}`,
    preview.complete
      ? `${formatCount(preview.sampledRows)} rows in the file`
      : `types inferred from the first ${formatCount(preview.sampledRows)} rows`,
  ];
  const d = preview.detected;
  if (d.encoding) parts.push(`encoding ${d.encoding}${d.bom ? " with BOM" : ""}`);
  if (d.sheet) parts.push(`sheet ${d.sheet}`);
  if (d.totalRows !== undefined) parts.push(`${formatCount(d.totalRows)} rows`);
  return parts.join(" · ");
}

function Warnings({ items, title }: { items: string[]; title: string }) {
  if (items.length === 0) return null;
  return (
    <div className="banner banner--warning" role="status">
      <p className="banner__title">{title}</p>
      <ul className="banner__list">
        {items.map((w) => (
          <li key={w}>{w}</li>
        ))}
      </ul>
    </div>
  );
}

function ErrorLog({ errors, truncated }: { errors: ImportRowError[]; truncated: boolean }) {
  if (errors.length === 0) return null;
  return (
    <div className="transfer-errors">
      <table className="transfer-table" aria-label="Error log">
        <thead>
          <tr>
            <th scope="col">Row</th>
            <th scope="col">Column</th>
            <th scope="col">Problem</th>
          </tr>
        </thead>
        <tbody>
          {errors.map((e, i) => (
            // biome-ignore lint/suspicious/noArrayIndexKey: a row may have several errors.
            <tr key={i}>
              <td>{formatCount(e.row)}</td>
              <td>{e.column ?? ""}</td>
              <td>{e.message}</td>
            </tr>
          ))}
        </tbody>
      </table>
      {truncated ? (
        <p className="field__hint">
          Only the first {formatCount(errors.length)} errors are listed.
        </p>
      ) : null}
    </div>
  );
}

// --- the wizard -----------------------------------------------------------------------------

export function ImportWizard({ init, onClose }: { init: ImportInit; onClose: () => void }) {
  const { sessions } = useConnections();
  const id = useId();
  const [step, setStep] = useState<Step>("file");
  const [file, setFile] = useState<PickedFile | null>(null);
  const [picking, setPicking] = useState(false);
  const [pickError, setPickError] = useState<string | null>(null);
  const [options, setOptions] = useState<ReadOptions>({ format: "csv", delimiter: "," });
  const [preview, setPreview] = useState<PreviewState>({ status: "idle" });
  const [target, setTarget] = useState<TargetForm>(() =>
    initialTarget(init, sessions[0]?.id ?? ""),
  );
  const [targetDef, setTargetDef] = useState<{ ref: ObjectRef; columns: ColumnDef[] } | null>(null);
  const [targetError, setTargetError] = useState<string | null>(null);
  const [loadingTarget, setLoadingTarget] = useState(false);
  const [rows, setRows] = useState<MappingRow[]>([]);
  const [rowsBasis, setRowsBasis] = useState("");
  const [showProblems, setShowProblems] = useState(false);
  const [conversion, setConversion] = useState<ConversionOptions>({
    trimText: false,
    emptyTextAsNull: false,
    truncateText: false,
  });
  const [errorPolicy, setErrorPolicy] = useState<ErrorPolicy>({ type: "stop" });
  const [maxErrors, setMaxErrors] = useState(100);
  const [transaction, setTransaction] = useState<TransactionPolicy>({ type: "auto" });
  const [commitRows, setCommitRows] = useState(100_000);
  const [plan, setPlan] = useState<PlanState>({ status: "idle" });
  const [confirmed, setConfirmed] = useState(false);
  const [run, dispatch] = useReducer(importRunReducer, IDLE_IMPORT);
  const cancelWanted = useRef(false);
  const alive = useRef(true);
  const fileRef = useRef<PickedFile | null>(null);
  fileRef.current = file;

  const session = sessions.find((s) => s.id === target.sessionId) ?? null;
  const mode = modeOf(target);
  const create = createsTable(mode);
  const optionsProblem = readOptionsProblem(options);
  const previewData =
    preview.status === "ok"
      ? preview.data
      : preview.status === "loading" || preview.status === "error"
        ? preview.data
        : null;
  const running = run.status === "starting" || run.status === "running";

  // The wizard owns its file handle: released when it closes or another file is picked.
  useEffect(() => {
    alive.current = true;
    return () => {
      alive.current = false;
      const f = fileRef.current;
      if (f) void fileRelease(f.handle).catch(() => {});
    };
  }, []);

  useEffect(() => {
    setTransferBusy(running);
    return () => setTransferBusy(false);
  }, [running]);

  // --- preview: asked again whenever the file or the options change -----------------------
  const optionsKey = JSON.stringify(options);
  const fileHandle = file?.handle ?? null;
  useEffect(() => {
    if (!fileHandle || optionsProblem) return;
    let current = true;
    const requestId = newRequestId();
    setPreview((p) => ({
      status: "loading",
      data: p.status === "idle" ? null : (p.data ?? null),
    }));
    const timer = setTimeout(() => {
      importPreview(fileHandle, JSON.parse(optionsKey) as ReadOptions, requestId).then(
        (data) => {
          if (current) setPreview({ status: "ok", data });
        },
        (error) => {
          if (!current) return;
          const message = toCommandError(error).message;
          setPreview((p) => ({
            status: "error",
            message,
            data: p.status === "idle" ? null : (p.data ?? null),
          }));
        },
      );
    }, PREVIEW_DELAY_MS);
    return () => {
      current = false;
      clearTimeout(timer);
      void cancelRequest(requestId).catch(() => {});
    };
  }, [fileHandle, optionsKey, optionsProblem]);

  // --- steps ------------------------------------------------------------------------------

  async function pickFile() {
    setPickError(null);
    setPicking(true);
    try {
      const picked = await filePickOpen(importFilters(), "Choose a file to import");
      if (!alive.current || !picked) return;
      const previous = file;
      if (previous) void fileRelease(previous.handle).catch(() => {});
      setFile(picked);
      setOptions(readOptionsFor(picked.name));
      setPreview({ status: "idle" });
      setRowsBasis("");
      setTarget((t) => (t.suggested ? { ...t, name: tableNameFromFile(picked.name) } : t));
      setStep("format");
    } catch (error) {
      if (alive.current) setPickError(toCommandError(error).message);
    } finally {
      if (alive.current) setPicking(false);
    }
  }

  function changeFormat(format: ImportFormat) {
    if (format === options.format) return;
    setOptions(defaultReadOptions(format));
  }

  async function toMapping() {
    setTargetError(null);
    if (!session) {
      setTargetError("Choose a connection.");
      return;
    }
    if (!target.name.trim()) {
      setTargetError(create ? "Enter the new table's name." : "Enter the table's name.");
      return;
    }
    const ref = targetRef(target);
    let def: { ref: ObjectRef; columns: ColumnDef[] } | null = null;
    if (!create) {
      setLoadingTarget(true);
      try {
        const loaded = await objectLoad(session.id, ref);
        if (loaded.kind !== "table") {
          setTargetError(`${qualifiedName(ref)} is not a table.`);
          return;
        }
        def = { ref: loaded.ref, columns: loaded.columns };
      } catch (error) {
        setTargetError(`Could not load ${qualifiedName(ref)}: ${toCommandError(error).message}`);
        return;
      } finally {
        if (alive.current) setLoadingTarget(false);
      }
    }
    if (!alive.current) return;
    setTargetDef(def);
    const columns = previewData?.columns ?? [];
    const basis = JSON.stringify([
      columns.map((c) => [c.name, typeKey(c.dataType)]),
      create,
      def?.columns.map((c) => c.name) ?? [],
    ]);
    if (basis !== rowsBasis) {
      setRows(initialMapping(columns, mode, def?.columns ?? []));
      setRowsBasis(basis);
      setShowProblems(false);
    }
    setStep("mapping");
  }

  const problems: MappingProblem[] =
    step === "mapping" || step === "summary"
      ? mappingProblems(rows, mode, targetDef?.columns ?? [])
      : [];

  function request() {
    const table = create ? targetRef(target) : (targetDef?.ref ?? targetRef(target));
    return buildImportRequest({
      options,
      rows,
      session: target.sessionId,
      table,
      mode,
      conversion,
      errors: errorPolicy.type === "skip" ? { type: "skip", maxErrors } : errorPolicy,
      transaction:
        transaction.type === "batches" ? { type: "batches", rows: commitRows } : transaction,
    });
  }

  async function toSummary() {
    setShowProblems(true);
    if (problems.length > 0) return;
    setStep("summary");
    setConfirmed(false);
    setPlan({ status: "loading" });
    if (!file) return;
    try {
      const result = await importPlan(file.handle, request());
      if (alive.current) setPlan({ status: "ok", plan: result });
    } catch (error) {
      if (alive.current) setPlan({ status: "error", message: toCommandError(error).message });
    }
  }

  async function start() {
    if (!file || plan.status !== "ok") return;
    cancelWanted.current = false;
    setStep("run");
    dispatch({ type: "start" });
    try {
      const importId = await importRun(file.handle, request(), (event) => {
        if (!alive.current) return;
        dispatch({ type: "event", event });
        if (isTerminalTransferEvent(event)) setTransferBusy(false);
      });
      if (!alive.current) return;
      dispatch({ type: "id", importId });
      if (cancelWanted.current) void importCancel(importId).catch(() => {});
    } catch (error) {
      if (alive.current) dispatch({ type: "startFailed", error: toCommandError(error) });
    }
  }

  function cancel() {
    dispatch({ type: "cancelling" });
    if (run.importId) void importCancel(run.importId).catch(() => {});
    else cancelWanted.current = true;
  }

  function close() {
    if (running) return;
    onClose();
  }

  function updateRow(source: number, patch: Partial<MappingRow>) {
    setRows((current) => current.map((r) => (r.source === source ? { ...r, ...patch } : r)));
  }

  // --- views ------------------------------------------------------------------------------

  const stepIndex = STEPS.findIndex((s) => s.id === step);

  function fileStep(): ReactNode {
    return (
      <div className="transfer-step">
        <p>
          Choose a CSV, TSV, JSON, Excel (XLSX) or Parquet file. The file is read where it is; it is
          never copied.
        </p>
        <div className="transfer-file">
          <button
            type="button"
            className="button button--primary"
            onClick={() => void pickFile()}
            disabled={picking}
          >
            {file ? "Choose another file…" : "Choose file…"}
          </button>
          {file ? (
            <span className="transfer-file__name" data-testid="picked-file">
              {file.name}
              {file.size !== undefined ? ` · ${formatBytes(file.size)}` : ""}
            </span>
          ) : null}
        </div>
        {pickError ? (
          <p className="field__error" role="alert">
            {pickError}
          </p>
        ) : null}
      </div>
    );
  }

  function formatStep(): ReactNode {
    return (
      <div className="transfer-step transfer-step--split">
        <div className="transfer-step__side">
          <div className="field">
            <label htmlFor={`${id}-format`}>Format</label>
            <select
              id={`${id}-format`}
              value={formatOf(options)}
              onChange={(e) => changeFormat(e.target.value as ImportFormat)}
            >
              {IMPORT_FORMATS.map((f) => (
                <option key={f} value={f}>
                  {FORMAT_LABEL[f]}
                </option>
              ))}
            </select>
          </div>
          <ReadOptionsForm options={options} preview={previewData} onChange={setOptions} />
        </div>
        <div className="transfer-step__main">
          <div className="transfer-step__status" role="status" data-testid="preview-status">
            {optionsProblem
              ? optionsProblem
              : preview.status === "loading"
                ? "Reading the file…"
                : preview.status === "ok"
                  ? previewSummary(preview.data)
                  : ""}
          </div>
          {preview.status === "error" ? (
            <p className="field__error" role="alert" data-testid="preview-error">
              Could not read the file: {preview.message}
            </p>
          ) : null}
          {previewData ? (
            <>
              <Warnings items={previewData.warnings ?? []} title="While reading the file" />
              <Warnings
                items={previewData.columns.flatMap((c) =>
                  (c.warnings ?? []).map((w) => `${c.name}: ${w}`),
                )}
                title="About the inferred types"
              />
              <PreviewGrid preview={previewData} />
            </>
          ) : null}
        </div>
      </div>
    );
  }

  function targetStep(): ReactNode {
    return (
      <div className="transfer-step">
        {sessions.length === 0 ? (
          <p className="field__error" role="alert">
            Open a connection first: the import goes into one of its tables.
          </p>
        ) : null}
        <div className="field">
          <label htmlFor={`${id}-session`}>Connection</label>
          <select
            id={`${id}-session`}
            value={target.sessionId}
            onChange={(e) => {
              setTarget({ ...target, sessionId: e.target.value });
              setTargetDef(null);
            }}
          >
            {session ? null : <option value="">Choose a connection</option>}
            {sessions.map((s) => (
              <option key={s.id} value={s.id}>
                {s.profile_name} ({ENGINE_INFO[s.engine].label})
              </option>
            ))}
          </select>
        </div>
        <fieldset className="fieldset">
          <legend className="visually-hidden">Target table</legend>
          <div className="segmented" role="radiogroup" aria-label="Target table">
            {(["new", "existing"] as const).map((kind) => (
              <label key={kind} className="segmented__item transfer-radio">
                <input
                  type="radio"
                  name={`${id}-kind`}
                  value={kind}
                  checked={target.kind === kind}
                  onChange={() => setTarget({ ...target, kind })}
                />
                {kind === "new" ? "New table" : "Existing table"}
              </label>
            ))}
          </div>
        </fieldset>
        <div className="field-row">
          <div className="field">
            <label htmlFor={`${id}-catalog`}>Database</label>
            <input
              id={`${id}-catalog`}
              type="text"
              value={target.catalog}
              placeholder="the connection's database"
              onChange={(e) => setTarget({ ...target, catalog: e.target.value })}
            />
          </div>
          <div className="field">
            <label htmlFor={`${id}-schema`}>Schema</label>
            <input
              id={`${id}-schema`}
              type="text"
              value={target.schema}
              placeholder="the default schema"
              onChange={(e) => setTarget({ ...target, schema: e.target.value })}
            />
          </div>
          <div className="field">
            <label htmlFor={`${id}-table`}>Table</label>
            <input
              id={`${id}-table`}
              type="text"
              value={target.name}
              onChange={(e) => setTarget({ ...target, name: e.target.value, suggested: false })}
            />
          </div>
        </div>
        {target.kind === "existing" ? (
          <div className="field">
            <label htmlFor={`${id}-mode`}>Before importing</label>
            <select
              id={`${id}-mode`}
              value={target.existingMode}
              onChange={(e) =>
                setTarget({
                  ...target,
                  existingMode: e.target.value as TargetForm["existingMode"],
                })
              }
            >
              {(["append", "truncate", "delete_all", "replace"] as const).map((m) => (
                <option key={m} value={m}>
                  {MODE_LABEL[m]}
                </option>
              ))}
            </select>
            {target.existingMode === "replace" ? (
              <p className="field__hint">
                The table is dropped and created again from the column mapping.
              </p>
            ) : null}
          </div>
        ) : null}
        {targetError ? (
          <p className="field__error" role="alert" data-testid="target-error">
            {targetError}
          </p>
        ) : null}
      </div>
    );
  }

  function mappingStep(): ReactNode {
    const byRow = new Map<number, string[]>();
    for (const p of problems) {
      if (p.source === undefined) continue;
      byRow.set(p.source, [...(byRow.get(p.source) ?? []), p.message]);
    }
    const targetColumns = targetDef?.columns ?? [];
    const sample = (index: number) => {
      const row = previewData?.rows.find((r) => r[index] !== null && r[index] !== undefined);
      return row?.[index] ?? "";
    };
    return (
      <div className="transfer-step">
        <div className="transfer-mapping">
          <table className="transfer-table" aria-label="Column mapping">
            <thead>
              <tr>
                <th scope="col">Import</th>
                <th scope="col">Source column</th>
                <th scope="col">Sample</th>
                <th scope="col">Type</th>
                <th scope="col">{create ? "New column" : "Target column"}</th>
                {create ? <th scope="col">Key</th> : null}
              </tr>
            </thead>
            <tbody>
              {rows.map((r) => {
                const rowProblems = showProblems ? (byRow.get(r.source) ?? []) : [];
                return (
                  <tr
                    key={r.source}
                    className={rowProblems.length > 0 ? "transfer-table__invalid" : undefined}
                  >
                    <td>
                      <input
                        type="checkbox"
                        aria-label={`Import ${r.sourceName}`}
                        checked={!r.skip}
                        onChange={(e) =>
                          updateRow(r.source, {
                            skip: !e.target.checked,
                            ...(e.target.checked ? {} : { primaryKey: false }),
                          })
                        }
                      />
                    </td>
                    <th scope="row">{r.sourceName}</th>
                    <td className="transfer-table__sample">{sample(r.source)}</td>
                    <td>
                      <select
                        aria-label={`Type of ${r.sourceName}`}
                        value={typeKey(r.dataType)}
                        disabled={r.skip}
                        onChange={(e) =>
                          updateRow(r.source, {
                            dataType: JSON.parse(e.target.value) as MappingRow["dataType"],
                          })
                        }
                      >
                        {typeChoices(r.inferred).map((t, i) => (
                          <option key={typeKey(t)} value={typeKey(t)}>
                            {typeLabel(t)}
                            {i === 0 ? " (inferred)" : ""}
                          </option>
                        ))}
                      </select>
                    </td>
                    <td>
                      {create ? (
                        <input
                          type="text"
                          aria-label={`New column for ${r.sourceName}`}
                          aria-invalid={rowProblems.length > 0}
                          value={r.target}
                          disabled={r.skip}
                          onChange={(e) => updateRow(r.source, { target: e.target.value })}
                        />
                      ) : (
                        <select
                          aria-label={`Target column for ${r.sourceName}`}
                          aria-invalid={rowProblems.length > 0}
                          value={r.target}
                          disabled={r.skip}
                          onChange={(e) => updateRow(r.source, { target: e.target.value })}
                        >
                          <option value="">Choose…</option>
                          {r.target && !targetColumns.some((c) => c.name === r.target) ? (
                            <option value={r.target}>{r.target}</option>
                          ) : null}
                          {targetColumns.map((c) => (
                            <option key={c.name} value={c.name}>
                              {c.name} ({c.native_type || typeLabel(c.data_type)})
                            </option>
                          ))}
                        </select>
                      )}
                      {rowProblems.map((m) => (
                        <p key={m} className="field__error">
                          {m}
                        </p>
                      ))}
                    </td>
                    {create ? (
                      <td>
                        <input
                          type="checkbox"
                          aria-label={`Primary key: ${r.sourceName}`}
                          checked={r.primaryKey}
                          disabled={r.skip}
                          onChange={(e) => updateRow(r.source, { primaryKey: e.target.checked })}
                        />
                      </td>
                    ) : null}
                  </tr>
                );
              })}
            </tbody>
          </table>
        </div>
        {showProblems && problems.length > 0 ? (
          <div className="banner banner--error" role="alert" data-testid="mapping-problems">
            <p className="banner__title">Fix the mapping first:</p>
            <ul className="banner__list">
              {problems.map((p) => (
                <li key={`${p.source ?? "all"}:${p.message}`}>{p.message}</li>
              ))}
            </ul>
          </div>
        ) : null}
        <div className="transfer-options">
          <fieldset className="fieldset">
            <legend>Bad rows</legend>
            <label className="checkbox">
              <input
                type="radio"
                name={`${id}-errors`}
                checked={errorPolicy.type === "stop"}
                onChange={() => setErrorPolicy({ type: "stop" })}
              />
              Stop at the first bad row
            </label>
            <label className="checkbox">
              <input
                type="radio"
                name={`${id}-errors`}
                checked={errorPolicy.type === "skip"}
                onChange={() => setErrorPolicy({ type: "skip", maxErrors })}
              />
              Skip bad rows, up to
              <input
                type="number"
                aria-label="Most rows to skip"
                className="transfer-form__number"
                min={0}
                value={maxErrors}
                disabled={errorPolicy.type !== "skip"}
                onChange={(e) => setMaxErrors(Math.max(0, Math.floor(Number(e.target.value) || 0)))}
              />
            </label>
          </fieldset>
          <fieldset className="fieldset">
            <legend>Transactions</legend>
            <select
              aria-label="Transactions"
              value={transaction.type}
              onChange={(e) =>
                setTransaction(
                  e.target.value === "batches"
                    ? { type: "batches", rows: commitRows }
                    : { type: e.target.value as "auto" | "single" },
                )
              }
            >
              <option value="auto">Automatic (one transaction for small files)</option>
              <option value="single">One transaction (all or nothing)</option>
              <option value="batches">Commit every N rows</option>
            </select>
            {transaction.type === "batches" ? (
              <label className="checkbox">
                Rows per commit
                <input
                  type="number"
                  className="transfer-form__number"
                  min={1}
                  value={commitRows}
                  onChange={(e) =>
                    setCommitRows(Math.max(1, Math.floor(Number(e.target.value) || 1)))
                  }
                />
              </label>
            ) : null}
          </fieldset>
          <fieldset className="fieldset">
            <legend>Values</legend>
            <label className="checkbox">
              <input
                type="checkbox"
                checked={conversion.trimText ?? false}
                onChange={(e) => setConversion({ ...conversion, trimText: e.target.checked })}
              />
              Trim spaces around text
            </label>
            <label className="checkbox">
              <input
                type="checkbox"
                checked={conversion.emptyTextAsNull ?? false}
                onChange={(e) =>
                  setConversion({ ...conversion, emptyTextAsNull: e.target.checked })
                }
              />
              Empty text is NULL
            </label>
            <label className="checkbox">
              <input
                type="checkbox"
                checked={conversion.truncateText ?? false}
                onChange={(e) => setConversion({ ...conversion, truncateText: e.target.checked })}
              />
              Cut text that is too long for its column
            </label>
          </fieldset>
        </div>
      </div>
    );
  }

  function summaryStep(): ReactNode {
    const table = create ? targetRef(target) : (targetDef?.ref ?? targetRef(target));
    return (
      <div className="transfer-step">
        <dl className="transfer-summary">
          <div>
            <dt>File</dt>
            <dd>
              {file?.name} ({FORMAT_LABEL[options.format]}
              {file?.size !== undefined ? `, ${formatBytes(file.size)}` : ""})
            </dd>
          </div>
          <div>
            <dt>Target</dt>
            <dd>
              {qualifiedName(table)} on {session?.profile_name ?? "?"}
            </dd>
          </div>
          <div>
            <dt>Mode</dt>
            <dd>{MODE_LABEL[mode]}</dd>
          </div>
          <div>
            <dt>Columns</dt>
            <dd>
              {rows.filter((r) => !r.skip).length} of {rows.length} imported
            </dd>
          </div>
          <div>
            <dt>Bad rows</dt>
            <dd>
              {errorPolicy.type === "stop"
                ? "Stop at the first one"
                : `Skip up to ${formatCount(maxErrors)}`}
            </dd>
          </div>
        </dl>
        {plan.status === "loading" ? <p role="status">Checking the import…</p> : null}
        {plan.status === "error" ? (
          <p className="field__error" role="alert" data-testid="plan-error">
            The import cannot run: {plan.message}
          </p>
        ) : null}
        {plan.status === "ok" ? (
          <>
            <dl className="transfer-summary">
              <div>
                <dt>Method</dt>
                <dd>{METHOD_LABEL[plan.plan.method] ?? plan.plan.method}</dd>
              </div>
              <div>
                <dt>Commits</dt>
                <dd>
                  {plan.plan.transaction.commitEvery === undefined
                    ? "One transaction"
                    : `Every ${formatCount(plan.plan.transaction.commitEvery)} rows`}
                </dd>
              </div>
            </dl>
            <Warnings items={plan.plan.warnings ?? []} title="Warnings" />
            {plan.plan.ddl.length > 0 ? (
              <div className="field">
                <span className="transfer-label">Statements run first</span>
                <pre className="transfer-ddl" data-testid="plan-ddl">
                  {plan.plan.ddl.join(";\n\n")}
                </pre>
              </div>
            ) : null}
            <table className="transfer-table" aria-label="Planned columns">
              <thead>
                <tr>
                  <th scope="col">Source</th>
                  <th scope="col">Target</th>
                  <th scope="col">Type</th>
                  <th scope="col">Conversion</th>
                </tr>
              </thead>
              <tbody>
                {plan.plan.columns.map((c) => (
                  <tr key={c.source}>
                    <td>{c.sourceName}</td>
                    <td>{c.target}</td>
                    <td>
                      {typeLabel(c.sourceType)} → {typeLabel(c.targetType)}
                    </td>
                    <td>{c.conversion ?? ""}</td>
                  </tr>
                ))}
              </tbody>
            </table>
            {plan.plan.destructive ? (
              <div className="banner banner--error" role="alert" data-testid="destructive-warning">
                <p className="banner__title">
                  This import cannot be fully undone: if it fails or is cancelled, the rows that
                  were in {qualifiedName(plan.plan.table)} may be gone.
                </p>
                <label className="checkbox">
                  <input
                    type="checkbox"
                    checked={confirmed}
                    onChange={(e) => setConfirmed(e.target.checked)}
                  />
                  I understand; import anyway
                </label>
              </div>
            ) : null}
          </>
        ) : null}
      </div>
    );
  }

  function runStep(): ReactNode {
    const p = run.progress;
    const report = run.report;
    const done = run.status === "finished" || run.status === "failed" || run.status === "cancelled";
    const table = plan.status === "ok" ? plan.plan.table : targetRef(target);
    const errorCount = Math.max(p?.rowsSkipped ?? 0, run.rowErrors.length);
    if (!done) {
      const r = rates({
        done: p?.bytesRead ?? 0,
        total: run.totalBytes || file?.size,
        rows: p?.rowsRead ?? 0,
        bytes: p?.bytesRead ?? 0,
        elapsedMs: p?.elapsedMs ?? 0,
      });
      return (
        <div className="transfer-step">
          <ProgressView
            label={`Importing ${file?.name ?? "the file"} into ${qualifiedName(table)}${run.cancelling ? " (cancelling…)" : ""}`}
            rates={r}
            unit="read"
            figures={[
              {
                label: "Read",
                value: `${formatBytes(p?.bytesRead ?? 0)} of ${formatBytes(run.totalBytes || file?.size || 0)}`,
              },
              { label: "Rows read", value: formatCount(p?.rowsRead ?? 0) },
              { label: "Rows inserted", value: formatCount(p?.rowsInserted ?? 0) },
              { label: "Rows committed", value: formatCount(p?.rowsCommitted ?? 0) },
              { label: "Errors", value: formatCount(errorCount), alert: errorCount > 0 },
            ]}
          />
          <ErrorLog errors={run.rowErrors} truncated={false} />
        </div>
      );
    }
    const title =
      run.status === "finished"
        ? "Import finished"
        : run.status === "cancelled"
          ? "Import cancelled"
          : "Import failed";
    return (
      <div className="transfer-step" data-testid="import-report">
        <div
          className={`banner ${run.status === "finished" ? "banner--ok" : run.status === "failed" ? "banner--error" : "banner--warning"}`}
          role={run.status === "failed" ? "alert" : "status"}
        >
          <p className="banner__title">{title}</p>
          {run.error ? <p>{run.error.message}</p> : null}
          {report && run.status !== "finished" ? <p>{report.leftBehind}</p> : null}
        </div>
        {report ? (
          <>
            <dl className="transfer-summary">
              <div>
                <dt>Table</dt>
                <dd>
                  {qualifiedName(report.table)}
                  {report.tableCreated ? " (created)" : ""}
                  {report.tableDropped ? " (dropped)" : ""}
                </dd>
              </div>
              <div>
                <dt>Rows read</dt>
                <dd data-testid="report-rows-read">{formatCount(report.rowsRead)}</dd>
              </div>
              <div>
                <dt>Rows committed</dt>
                <dd data-testid="report-rows-committed">{formatCount(report.rowsCommitted)}</dd>
              </div>
              <div>
                <dt>Rows skipped</dt>
                <dd data-testid="report-rows-skipped">{formatCount(report.rowsSkipped)}</dd>
              </div>
              <div>
                <dt>Read</dt>
                <dd>{formatBytes(report.bytesRead)}</dd>
              </div>
              <div>
                <dt>Time</dt>
                <dd>{formatMs(report.elapsedMs)}</dd>
              </div>
              <div>
                <dt>Method</dt>
                <dd>{METHOD_LABEL[report.method] ?? report.method}</dd>
              </div>
            </dl>
            <Warnings items={report.warnings ?? []} title="Warnings" />
            <ErrorLog errors={report.errors} truncated={report.errorsTruncated} />
          </>
        ) : (
          <ErrorLog errors={run.rowErrors} truncated={false} />
        )}
      </div>
    );
  }

  // --- footer -----------------------------------------------------------------------------

  const back = (to: Step) => (
    <button type="button" className="button" onClick={() => setStep(to)}>
      Back
    </button>
  );
  let footer: ReactNode;
  switch (step) {
    case "file":
      footer = (
        <>
          <button type="button" className="button" onClick={close}>
            Cancel
          </button>
          <button
            type="button"
            className="button button--primary"
            disabled={!file}
            onClick={() => setStep("format")}
          >
            Next
          </button>
        </>
      );
      break;
    case "format":
      footer = (
        <>
          <button type="button" className="button modal__footer-start" onClick={close}>
            Cancel
          </button>
          {back("file")}
          <button
            type="button"
            className="button button--primary"
            disabled={preview.status !== "ok" || optionsProblem !== null}
            onClick={() => setStep("target")}
          >
            Next
          </button>
        </>
      );
      break;
    case "target":
      footer = (
        <>
          <button type="button" className="button modal__footer-start" onClick={close}>
            Cancel
          </button>
          {back("format")}
          <button
            type="button"
            className="button button--primary"
            disabled={loadingTarget || !session}
            onClick={() => void toMapping()}
          >
            {loadingTarget ? "Loading the table…" : "Next"}
          </button>
        </>
      );
      break;
    case "mapping":
      footer = (
        <>
          <button type="button" className="button modal__footer-start" onClick={close}>
            Cancel
          </button>
          {back("target")}
          <button type="button" className="button button--primary" onClick={() => void toSummary()}>
            Next
          </button>
        </>
      );
      break;
    case "summary":
      footer = (
        <>
          <button type="button" className="button modal__footer-start" onClick={close}>
            Cancel
          </button>
          {back("mapping")}
          <button
            type="button"
            className="button button--primary"
            data-testid="start-import"
            disabled={plan.status !== "ok" || (plan.plan.destructive && !confirmed)}
            onClick={() => void start()}
          >
            Start import
          </button>
        </>
      );
      break;
    case "run":
      footer = running ? (
        <button
          type="button"
          className="button button--danger"
          data-testid="cancel-import"
          disabled={run.cancelling}
          onClick={cancel}
        >
          {run.cancelling ? "Cancelling…" : "Cancel import"}
        </button>
      ) : (
        <>
          {run.status === "finished" && session && plan.status === "ok" ? (
            <button
              type="button"
              className="button modal__footer-start"
              onClick={() => {
                openTableData({ sessionId: session.id, table: plan.plan.table });
                onClose();
              }}
            >
              Open table
            </button>
          ) : null}
          <button type="button" className="button button--primary" onClick={onClose}>
            Close
          </button>
        </>
      );
      break;
  }

  const body: Record<Step, () => ReactNode> = {
    file: fileStep,
    format: formatStep,
    target: targetStep,
    mapping: mappingStep,
    summary: summaryStep,
    run: runStep,
  };

  return (
    <Modal title="Import data" onClose={close} className="modal--transfer" footer={footer}>
      <ol className="transfer-steps" aria-label="Steps">
        {STEPS.map((s, i) => (
          <li
            key={s.id}
            className={`transfer-steps__item${i < stepIndex ? " transfer-steps__item--done" : ""}`}
            aria-current={s.id === step ? "step" : undefined}
          >
            {s.label}
          </li>
        ))}
      </ol>
      {body[step]()}
    </Modal>
  );
}
