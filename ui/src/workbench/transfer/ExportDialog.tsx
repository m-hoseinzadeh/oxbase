/**
 * The export dialog (docs/m5-contracts.md §5.4): format and options, then the native save
 * dialog (shown by Rust; the dialog only gets an opaque handle), then progress with cancel and
 * the report. Sources: a result of a run (every row), a table (with an optional filter and
 * sort) or a query.
 */
import { type ReactNode, useEffect, useId, useReducer, useRef, useState } from "react";
import {
  type ErrorInfo,
  type ExportEvent,
  type ExportReport,
  type ExportRequest,
  type ExportSource,
  exportCancel,
  exportRun,
  type FileFormat,
  filePickSave,
  fileRelease,
  filtersFor,
  isTerminalTransferEvent,
  type WriteOptions,
} from "../../api";
import { Modal } from "../../components/Modal";
import { toCommandError } from "../../connections/api";
import { formatMs } from "../format";
import {
  defaultWriteOptions,
  EXPORT_FORMATS,
  extensionFor,
  FORMAT_LABEL,
  formatBytes,
  formatCount,
  rates,
  writeOptionsProblems,
} from "./model";
import { ProgressView } from "./ProgressView";
import { type ExportInit, setTransferBusy } from "./store";
import { WriteOptionsForm } from "./WriteOptionsForm";

type ProgressEvent = Extract<ExportEvent, { type: "progress" }>;

export interface ExportRunState {
  status: "idle" | "picking" | "starting" | "running" | "finished" | "failed" | "cancelled";
  exportId: string | null;
  totalRows: number | null;
  progress: ProgressEvent | null;
  report: ExportReport | null;
  error: ErrorInfo | null;
  elapsedMs: number | null;
  cancelling: boolean;
}

const IDLE_EXPORT: ExportRunState = {
  status: "idle",
  exportId: null,
  totalRows: null,
  progress: null,
  report: null,
  error: null,
  elapsedMs: null,
  cancelling: false,
};

type Action =
  | { type: "picking" }
  | { type: "picked"; cancelled: boolean }
  | { type: "id"; exportId: string }
  | { type: "event"; event: ExportEvent }
  | { type: "startFailed"; error: ErrorInfo }
  | { type: "cancelling" };

function applyEvent(state: ExportRunState, event: ExportEvent): ExportRunState {
  switch (event.type) {
    case "started":
      return {
        ...state,
        status: state.status === "starting" ? "running" : state.status,
        exportId: event.exportId,
        totalRows: event.totalRows ?? state.totalRows,
      };
    case "progress":
      return { ...state, progress: event, totalRows: event.totalRows ?? state.totalRows };
    case "finished":
      return {
        ...state,
        status: "finished",
        report: event.report,
        elapsedMs: event.report.elapsedMs,
      };
    case "failed":
      return { ...state, status: "failed", error: event.error, elapsedMs: event.elapsedMs };
    case "cancelled":
      return { ...state, status: "cancelled", elapsedMs: event.elapsedMs };
  }
}

export function exportRunReducer(state: ExportRunState, action: Action): ExportRunState {
  switch (action.type) {
    case "picking":
      return { ...IDLE_EXPORT, status: "picking", totalRows: state.totalRows };
    case "picked":
      return action.cancelled ? { ...state, status: "idle" } : { ...state, status: "starting" };
    case "id":
      return { ...state, exportId: action.exportId };
    case "event":
      return applyEvent(state, action.event);
    case "startFailed":
      return { ...state, status: "failed", error: action.error };
    case "cancelling":
      return { ...state, cancelling: true };
  }
}

/** The source with the dialog's filter and sort (table exports from the tree). */
export function withFilter(
  source: ExportSource,
  filter: string,
  sortColumn: string,
  descending: boolean,
): ExportSource {
  if (source.type !== "table") return source;
  const sql = filter.trim();
  const column = sortColumn.trim();
  return {
    ...source,
    query: {
      ...source.query,
      ...(sql ? { filter: { type: "rawSql", sql } } : {}),
      ...(column ? { sort: [{ column, descending }] } : {}),
    },
  };
}

export function ExportDialog({ init, onClose }: { init: ExportInit; onClose: () => void }) {
  const id = useId();
  const [format, setFormat] = useState<FileFormat>("csv");
  const [byFormat, setByFormat] = useState<Partial<Record<FileFormat, WriteOptions>>>({});
  const [filter, setFilter] = useState("");
  const [sortColumn, setSortColumn] = useState("");
  const [descending, setDescending] = useState(false);
  const [showProblems, setShowProblems] = useState(false);
  const [run, dispatch] = useReducer(exportRunReducer, {
    ...IDLE_EXPORT,
    totalRows: init.totalRows ?? null,
  });
  const alive = useRef(true);
  const cancelWanted = useRef(false);

  const options = byFormat[format] ?? defaultWriteOptions(format, init.engine);
  const problems = writeOptionsProblems(options);
  const running = run.status === "picking" || run.status === "starting" || run.status === "running";
  const done = run.status === "finished" || run.status === "failed" || run.status === "cancelled";
  const sourceTable = init.source.type === "table" ? init.source.query.table.name : "export";

  useEffect(() => {
    alive.current = true;
    return () => {
      alive.current = false;
    };
  }, []);

  useEffect(() => {
    setTransferBusy(running);
    return () => setTransferBusy(false);
  }, [running]);

  function setOptions(next: WriteOptions) {
    setByFormat((current) => ({ ...current, [next.format]: next }));
  }

  async function start() {
    setShowProblems(true);
    if (problems.length > 0) return;
    cancelWanted.current = false;
    dispatch({ type: "picking" });
    let handle: string | null = null;
    try {
      const picked = await filePickSave(
        `${init.baseName}.${extensionFor(options)}`,
        filtersFor(format),
        "Export to",
      );
      if (!alive.current) return;
      dispatch({ type: "picked", cancelled: picked === null });
      if (!picked) return;
      handle = picked.handle;
      const request: ExportRequest = {
        source: init.offerFilter
          ? withFilter(init.source, filter, sortColumn, descending)
          : init.source,
        options,
      };
      const exportId = await exportRun(picked.handle, request, (event) => {
        if (!alive.current) return;
        dispatch({ type: "event", event });
        if (isTerminalTransferEvent(event)) setTransferBusy(false);
      });
      if (!alive.current) return;
      dispatch({ type: "id", exportId });
      if (cancelWanted.current) void exportCancel(exportId).catch(() => {});
    } catch (error) {
      // A handle the export did not take is dropped.
      if (handle) void fileRelease(handle).catch(() => {});
      if (alive.current) dispatch({ type: "startFailed", error: toCommandError(error) });
    }
  }

  function cancel() {
    dispatch({ type: "cancelling" });
    if (run.exportId) void exportCancel(run.exportId).catch(() => {});
    else cancelWanted.current = true;
  }

  function close() {
    if (running) return;
    onClose();
  }

  let body: ReactNode;
  if (run.status === "starting" || run.status === "running") {
    const p = run.progress;
    const total = run.totalRows;
    body = (
      <ProgressView
        label={`Exporting ${init.label}${run.cancelling ? " (cancelling…)" : ""}`}
        unit="written"
        rates={rates({
          done: p?.rows ?? 0,
          total,
          rows: p?.rows ?? 0,
          bytes: p?.bytesWritten ?? 0,
          elapsedMs: p?.elapsedMs ?? 0,
        })}
        figures={[
          {
            label: "Rows",
            value:
              total === null
                ? formatCount(p?.rows ?? 0)
                : `${formatCount(p?.rows ?? 0)} of ${formatCount(total)}`,
          },
          { label: "Written", value: formatBytes(p?.bytesWritten ?? 0) },
        ]}
      />
    );
  } else if (done) {
    const report = run.report;
    body = (
      <div className="transfer-step" data-testid="export-report">
        <div
          className={`banner ${run.status === "finished" ? "banner--ok" : run.status === "failed" ? "banner--error" : "banner--warning"}`}
          role={run.status === "failed" ? "alert" : "status"}
        >
          <p className="banner__title">
            {run.status === "finished"
              ? "Export finished"
              : run.status === "cancelled"
                ? "Export cancelled: no file was written."
                : "Export failed: no file was written."}
          </p>
          {run.error ? <p>{run.error.message}</p> : null}
        </div>
        {report ? (
          <>
            <dl className="transfer-summary">
              <div>
                <dt>File</dt>
                <dd data-testid="report-file">{report.fileName}</dd>
              </div>
              <div>
                <dt>Rows</dt>
                <dd data-testid="report-rows">{formatCount(report.rows)}</dd>
              </div>
              <div>
                <dt>Size</dt>
                <dd>{formatBytes(report.bytes)}</dd>
              </div>
              {report.sheets && report.sheets.length > 0 ? (
                <div>
                  <dt>Sheets</dt>
                  <dd>{report.sheets.join(", ")}</dd>
                </div>
              ) : null}
              <div>
                <dt>Time</dt>
                <dd>{formatMs(report.elapsedMs)}</dd>
              </div>
            </dl>
            {report.sourceTruncated ? (
              <div className="banner banner--warning" role="status">
                The result was cut at the row limit of its run: only the rows it kept were exported.
                Export the query to get every row.
              </div>
            ) : null}
            {(report.warnings ?? []).length > 0 ? (
              <ul className="banner banner--warning banner__list">
                {(report.warnings ?? []).map((w) => (
                  <li key={w}>{w}</li>
                ))}
              </ul>
            ) : null}
          </>
        ) : null}
      </div>
    );
  } else {
    body = (
      <div className="transfer-step">
        <p className="transfer-source" data-testid="export-source">
          {init.label}
          {init.totalRows !== undefined ? ` · ${formatCount(init.totalRows)} rows` : ""}
        </p>
        <div className="field">
          <label htmlFor={`${id}-format`}>Format</label>
          <select
            id={`${id}-format`}
            value={format}
            disabled={running}
            onChange={(e) => {
              setFormat(e.target.value as FileFormat);
              setShowProblems(false);
            }}
          >
            {EXPORT_FORMATS.map((f) => (
              <option key={f} value={f}>
                {FORMAT_LABEL[f]}
              </option>
            ))}
          </select>
        </div>
        <WriteOptionsForm
          options={options}
          onChange={setOptions}
          disabled={running}
          defaultTable={sourceTable}
        />
        {init.offerFilter && init.source.type === "table" ? (
          <fieldset className="fieldset">
            <legend>Rows</legend>
            <div className="field">
              <label htmlFor={`${id}-filter`}>Filter (SQL condition)</label>
              <input
                id={`${id}-filter`}
                type="text"
                placeholder="all rows"
                value={filter}
                disabled={running}
                onChange={(e) => setFilter(e.target.value)}
              />
            </div>
            <div className="field-row">
              <div className="field">
                <label htmlFor={`${id}-sort`}>Sort by column</label>
                <input
                  id={`${id}-sort`}
                  type="text"
                  placeholder="table order"
                  value={sortColumn}
                  disabled={running}
                  onChange={(e) => setSortColumn(e.target.value)}
                />
              </div>
              <label className="checkbox transfer-form__end">
                <input
                  type="checkbox"
                  checked={descending}
                  disabled={running || !sortColumn.trim()}
                  onChange={(e) => setDescending(e.target.checked)}
                />
                Descending
              </label>
            </div>
          </fieldset>
        ) : null}
        {showProblems && problems.length > 0 ? (
          <div className="banner banner--error" role="alert" data-testid="export-problems">
            <ul className="banner__list">
              {problems.map((p) => (
                <li key={p}>{p}</li>
              ))}
            </ul>
          </div>
        ) : null}
      </div>
    );
  }

  const footer =
    run.status === "starting" || run.status === "running" ? (
      <button
        type="button"
        className="button button--danger"
        data-testid="cancel-export"
        disabled={run.cancelling}
        onClick={cancel}
      >
        {run.cancelling ? "Cancelling…" : "Cancel export"}
      </button>
    ) : done ? (
      <button type="button" className="button button--primary" onClick={onClose}>
        Close
      </button>
    ) : (
      <>
        <button type="button" className="button" onClick={close} disabled={running}>
          Cancel
        </button>
        <button
          type="button"
          className="button button--primary"
          data-testid="start-export"
          disabled={running}
          onClick={() => void start()}
        >
          {run.status === "picking" ? "Choosing the file…" : "Export…"}
        </button>
      </>
    );

  return (
    <Modal title="Export data" onClose={close} className="modal--transfer-narrow" footer={footer}>
      {body}
    </Modal>
  );
}
