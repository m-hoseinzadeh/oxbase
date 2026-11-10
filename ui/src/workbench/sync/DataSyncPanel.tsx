/**
 * Data sync (docs/m8-contracts.md §6, §8.4): pick a source and a target database or schema of
 * the same engine; tables are paired by name (a key is the table's primary or unique key, or
 * columns typed by the user; tables without one are refused with the reason). Compare streams
 * per-table progress; each table shows its insert / update / delete counts and sample rows with
 * the changed cells highlighted. The script preview and the apply (deletes only when asked and
 * confirmed) come next; the core re-compares after applying.
 */
import {
  ArrowRight,
  Check,
  CircleX,
  FileCode,
  GitCompareArrows,
  LoaderCircle,
  Play,
  TriangleAlert,
} from "lucide-react";
import { type ReactNode, useEffect, useId, useReducer, useRef, useState } from "react";
import {
  compareForget,
  type DataScript,
  dataApply,
  dataCancel,
  dataCompare,
  dataScript,
  type ObjectRef,
  type RowOp,
  type RowSample,
  type TableDataDiff,
  type TablePair,
} from "../../api";
import { ICON, ICON_LG, ICON_SM } from "../../components/icons";
import { Modal } from "../../components/Modal";
import { Select } from "../../components/Select";
import { newRequestId, toCommandError } from "../../connections/api";
import { useConnections } from "../../connections/store";
import { ENGINE_INFO } from "../../connections/types";
import { languageFor } from "../editor/language";
import { SqlEditor, type SqlEditorHandle } from "../editor/SqlEditor";
import { formatMs } from "../format";
import type { SyncPanelParams } from "../layout";
import { engineFamily } from "../nosql/engines";
import { ContainerPicker, type SideValue, sideFromInit } from "./ContainerPicker";
import { listTables } from "./catalog";
import {
  autoPairs,
  compareStatusText,
  containerLabel,
  dataApplyReducer,
  dataCompareReducer,
  dataTotals,
  ended,
  IDLE_DATA_APPLY,
  IDLE_DATA_COMPARE,
  type PairRow,
  parseKeyColumns,
  plural,
  qualifiedName,
  sameRef,
} from "./model";
import { Acknowledge, Note, RunBar, WarningList } from "./parts";

type TablesState =
  | { status: "idle" }
  | { status: "loading" }
  | { status: "ok"; targets: ObjectRef[] }
  | { status: "error"; message: string };

type ScriptState =
  | { status: "idle" }
  | { status: "loading"; requestId: string }
  | { status: "ok"; script: DataScript }
  | { status: "error"; message: string };

/** The sides a compare ran on: what its script and apply act on, whatever the pickers say now. */
interface ComparedSides {
  sourceSession: string;
  source: ObjectRef;
  targetSession: string;
  target: ObjectRef;
}

const OP_LABEL: Record<RowOp, string> = { insert: "Inserts", update: "Updates", delete: "Deletes" };

function cell(value: string | null | undefined): ReactNode {
  if (value === null) return <span className="transfer-table__null">NULL</span>;
  return value ?? "";
}

/** The preview as a script: the session statements around the statements (the inlined
 * literals read back as written only in that session), each ended so the lines read as a
 * script. */
function scriptText(script: DataScript): string {
  return [...(script.prologue ?? []), ...script.statements, ...(script.epilogue ?? [])]
    .map((s) => (/;\s*$|\bGO\s*$/i.test(s) ? s : `${s};`))
    .join("\n");
}

export function DataSyncPanel({ params }: { params: SyncPanelParams }) {
  const { sessions } = useConnections();
  const id = useId();
  const sqlSessions = sessions.filter((s) => engineFamily(s.engine) === "sql");
  const [source, setSource] = useState<SideValue>(() =>
    sideFromInit(params.source, sqlSessions[0]?.id ?? ""),
  );
  const [target, setTarget] = useState<SideValue>(() =>
    sideFromInit(
      params.target ?? (params.source ? { sessionId: params.source.sessionId } : undefined),
    ),
  );
  const [tables, setTables] = useState<TablesState>({ status: "idle" });
  const [pairs, setPairs] = useState<PairRow[]>([]);
  const [chunkRows, setChunkRows] = useState(10_000);
  const [sampleRows, setSampleRows] = useState(20);
  const [useChunkHash, setUseChunkHash] = useState(true);
  const [requested, setRequested] = useState<PairRow[]>([]);
  const [compare, dispatchCompare] = useReducer(dataCompareReducer, IDLE_DATA_COMPARE);
  const [activePair, setActivePair] = useState<number | null>(null);
  const [sampleOp, setSampleOp] = useState<RowOp>("insert");
  const [ops, setOps] = useState({ inserts: true, updates: true, deletes: false });
  const [script, setScript] = useState<ScriptState>({ status: "idle" });
  const [compared, setCompared] = useState<ComparedSides | null>(null);
  // The preview request whose answer may still show (a changed choice drops the pending one).
  const scriptRequest = useRef<string | null>(null);
  const [confirming, setConfirming] = useState(false);
  const [acknowledged, setAcknowledged] = useState(false);
  const [apply, dispatchApply] = useReducer(dataApplyReducer, IDLE_DATA_APPLY);
  const scriptEditor = useRef<SqlEditorHandle | null>(null);
  const compareIdRef = useRef<string | null>(null);
  const runs = useRef<{ compare: string | null; apply: string | null }>({
    compare: null,
    apply: null,
  });
  const alive = useRef(true);

  const sourceSession = sqlSessions.find((s) => s.id === source.sessionId) ?? null;
  const engine = sourceSession?.engine;
  const targetSessions = sqlSessions.filter((s) => !engine || s.engine === engine);
  const comparing = compare.status === "starting" || compare.status === "running";
  const applying =
    apply.status === "starting" || apply.status === "running" || apply.status === "recomparing";
  const busy = comparing || applying;
  // Source and target picked as the same database or schema: nothing to compare.
  const sameContainer =
    source.sessionId !== "" &&
    source.sessionId === target.sessionId &&
    sameRef(source.container, target.container);
  // The pickers moved away from the sides of the compare on screen: its script and apply would
  // still act on the compared target, so they wait for a new compare.
  const stale =
    compared !== null &&
    (compared.sourceSession !== source.sessionId ||
      compared.targetSession !== target.sessionId ||
      !sameRef(compared.source, source.container) ||
      !sameRef(compared.target, target.container));
  // Scripts and applies work on a complete, current compare only.
  const compareId = compare.status === "finished" && !stale ? compare.compareId : null;
  const comparedTarget = compared
    ? `${sqlSessions.find((s) => s.id === compared.targetSession)?.profile_name ?? "?"} · ${containerLabel(compared.target)}`
    : "";
  const results = Object.values(compare.results).sort((a, b) => a.pair - b.pair);
  const totals = dataTotals(results);
  const active = activePair === null ? null : (compare.results[activePair] ?? null);
  const language = languageFor(engine);
  const sourceKey = source.container ? JSON.stringify(source.container) : "";
  const targetKey = target.container ? JSON.stringify(target.container) : "";

  useEffect(() => {
    alive.current = true;
    return () => {
      alive.current = false;
      if (runs.current.compare) void dataCancel(runs.current.compare).catch(() => {});
      if (runs.current.apply) void dataCancel(runs.current.apply).catch(() => {});
      const last = compareIdRef.current;
      if (last) void compareForget(last).catch(() => {});
    };
  }, []);

  // A compare replaced by another (or by the apply's re-compare) is dropped from the core.
  useEffect(() => {
    const previous = compareIdRef.current;
    compareIdRef.current = compare.compareId;
    if (previous && previous !== compare.compareId) void compareForget(previous).catch(() => {});
  }, [compare.compareId]);

  // Both sides' tables, paired by name.
  useEffect(() => {
    if (!sourceKey || !targetKey) {
      setTables({ status: "idle" });
      setPairs([]);
      return;
    }
    let current = true;
    setTables({ status: "loading" });
    const sourceRef = JSON.parse(sourceKey) as ObjectRef;
    const targetRef = JSON.parse(targetKey) as ObjectRef;
    Promise.all([
      listTables(source.sessionId, sourceRef, false, newRequestId()),
      listTables(target.sessionId, targetRef, false, newRequestId()),
    ]).then(
      ([s, t]) => {
        if (!current) return;
        setTables({ status: "ok", targets: t });
        setPairs(autoPairs(s, t));
      },
      (error) => current && setTables({ status: "error", message: toCommandError(error).message }),
    );
    return () => {
      current = false;
    };
  }, [source.sessionId, target.sessionId, sourceKey, targetKey]);

  function setPair(index: number, patch: Partial<PairRow>) {
    setPairs(pairs.map((p, i) => (i === index ? { ...p, ...patch } : p)));
  }

  const chosen = pairs.filter((p) => p.include && p.target !== null);
  const canCompare =
    !busy &&
    chosen.length > 0 &&
    source.container !== null &&
    target.container !== null &&
    !sameContainer;

  /** New insert / update / delete choices: a preview of the old ones no longer applies. */
  function changeOps(patch: Partial<typeof ops>) {
    setOps({ ...ops, ...patch });
    scriptRequest.current = null;
    setScript({ status: "idle" });
  }

  async function runCompare() {
    if (!canCompare || !source.container || !target.container) return;
    const request: TablePair[] = chosen.map((p) => {
      const columns = parseKeyColumns(p.keyColumns);
      return {
        source: p.source,
        target: p.target as ObjectRef,
        ...(columns ? { key: { type: "columns", columns } } : {}),
      };
    });
    setRequested(chosen);
    setCompared({
      sourceSession: source.sessionId,
      source: source.container,
      targetSession: target.sessionId,
      target: target.container,
    });
    scriptRequest.current = null;
    setScript({ status: "idle" });
    dispatchApply({ type: "reset" });
    dispatchCompare({ type: "start" });
    setActivePair(null);
    try {
      const opId = await dataCompare(
        {
          sourceSession: source.sessionId,
          targetSession: target.sessionId,
          pairs: request,
          options: { chunkRows, sampleRows, useChunkHash },
        },
        (event) => {
          if (!alive.current) return;
          dispatchCompare({ type: "event", event });
          if (event.type === "tableFinished") {
            setActivePair((current) => current ?? event.table.pair);
          }
          if (ended(event.type)) runs.current.compare = null;
        },
      );
      runs.current.compare = opId;
      if (alive.current) dispatchCompare({ type: "started", id: opId });
    } catch (error) {
      if (alive.current) dispatchCompare({ type: "startFailed", error: toCommandError(error) });
    }
  }

  function cancelCompare() {
    if (!compare.id) return;
    dispatchCompare({ type: "cancelling" });
    void dataCancel(compare.id).catch(() => {});
  }

  async function previewScript() {
    if (!compareId) return;
    const requestId = newRequestId();
    scriptRequest.current = requestId;
    setScript({ status: "loading", requestId });
    const shown = () => alive.current && scriptRequest.current === requestId;
    try {
      const s = await dataScript({ compareId, options: { ...ops, previewLimit: 1000 } }, requestId);
      if (shown()) setScript({ status: "ok", script: s });
    } catch (error) {
      if (shown()) setScript({ status: "error", message: toCommandError(error).message });
    }
  }

  async function runApply() {
    if (!compareId) return;
    setConfirming(false);
    dispatchApply({ type: "start" });
    try {
      const opId = await dataApply(
        {
          compareId,
          options: {
            script: { ...ops },
            batchStatements: 1000,
            confirmDeletes: ops.deletes && acknowledged,
          },
        },
        (event) => {
          if (!alive.current) return;
          dispatchApply({ type: "event", event });
          if (event.type === "finished" || event.type === "failed" || event.type === "cancelled") {
            runs.current.apply = null;
            const recompare = event.report.recompare;
            if (recompare) {
              dispatchCompare({ type: "adopt", result: recompare });
              scriptRequest.current = null;
              setScript({ status: "idle" });
            }
          }
        },
      );
      runs.current.apply = opId;
      if (alive.current) dispatchApply({ type: "started", id: opId });
    } catch (error) {
      if (alive.current) dispatchApply({ type: "startFailed", error: toCommandError(error) });
    } finally {
      setAcknowledged(false);
    }
  }

  function cancelApply() {
    if (!apply.id) return;
    dispatchApply({ type: "cancelling" });
    void dataCancel(apply.id).catch(() => {});
  }

  // --- views ------------------------------------------------------------------------------

  function pairTable(): ReactNode {
    if (tables.status === "idle") return null;
    if (tables.status === "loading") {
      return (
        <p className="sync__running" role="status">
          <LoaderCircle {...ICON_SM} className="spin" />
          Listing the tables of both sides…
        </p>
      );
    }
    if (tables.status === "error") {
      return (
        <Note tone="error" title="Could not list the tables">
          {tables.message}
        </Note>
      );
    }
    if (pairs.length === 0) return <p className="sync__empty">The source has no tables.</p>;
    const targets = tables.targets;
    return (
      <div className="sync-pairs">
        <table className="transfer-table" data-testid="data-pairs" aria-label="Table pairs">
          <thead>
            <tr>
              <th scope="col">
                <input
                  type="checkbox"
                  aria-label="Compare every paired table"
                  checked={pairs.every((p) => p.include || p.target === null)}
                  disabled={busy}
                  onChange={(e) =>
                    setPairs(
                      pairs.map((p) => ({ ...p, include: e.target.checked && p.target !== null })),
                    )
                  }
                />
              </th>
              <th scope="col">Source table</th>
              <th scope="col">Target table</th>
              <th scope="col">Key columns</th>
            </tr>
          </thead>
          <tbody>
            {pairs.map((p, i) => (
              <tr key={qualifiedName(p.source)} data-testid="data-pair" data-table={p.source.name}>
                <td>
                  <input
                    type="checkbox"
                    aria-label={`Compare ${p.source.name}`}
                    checked={p.include}
                    disabled={busy || p.target === null}
                    onChange={(e) => setPair(i, { include: e.target.checked })}
                  />
                </td>
                <th scope="row" className="transfer-table__name">
                  {qualifiedName(p.source)}
                </th>
                <td>
                  <Select
                    aria-label={`Target of ${p.source.name}`}
                    value={p.target?.name ?? ""}
                    disabled={busy}
                    onChange={(e) => {
                      const t = targets.find((x) => x.name === e.target.value) ?? null;
                      setPair(i, { target: t, include: t !== null, caseFolded: false });
                    }}
                  >
                    <option value="">No target table</option>
                    {targets.map((t) => (
                      <option key={t.name} value={t.name}>
                        {qualifiedName(t)}
                      </option>
                    ))}
                  </Select>
                  {p.caseFolded ? (
                    <span
                      className="badge badge--warning sync-badge"
                      data-testid="pair-case-folded"
                      title="The names differ in case: tick the row if this is the same table."
                    >
                      <TriangleAlert {...ICON_SM} />
                      matched ignoring case
                    </span>
                  ) : null}
                </td>
                <td>
                  <input
                    type="text"
                    aria-label={`Key columns of ${p.source.name}`}
                    placeholder="primary or unique key"
                    value={p.keyColumns}
                    disabled={busy}
                    onChange={(e) => setPair(i, { keyColumns: e.target.value })}
                  />
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
    );
  }

  function resultRow(t: TableDataDiff): ReactNode {
    const live = compare.progress[t.pair];
    const c = t.status.type === "compared" ? t.counts : (live?.counts ?? t.counts);
    const status = compareStatusText(t);
    const tone =
      t.status.type === "compared"
        ? c.inserts + c.updates + c.deletes === 0
          ? "success"
          : "warning"
        : t.status.type === "cancelled"
          ? "info"
          : "danger";
    return (
      <tr
        key={t.pair}
        data-testid="data-table-row"
        data-table={t.source.name}
        className={t.pair === activePair ? "sync-row--active" : undefined}
      >
        <th scope="row" className="transfer-table__name">
          <button type="button" className="sync-link" onClick={() => setActivePair(t.pair)}>
            {qualifiedName(t.source)}
          </button>
        </th>
        <td>
          <span className={`badge badge--${tone}`} title={status}>
            {status.length > 60 ? `${status.slice(0, 57)}…` : status}
          </span>
        </td>
        <td className="transfer-table__num">{c.sourceRows.toLocaleString("en-US")}</td>
        <td className="transfer-table__num">{c.targetRows.toLocaleString("en-US")}</td>
        <td className="transfer-table__num" data-testid="count-inserts">
          {c.inserts.toLocaleString("en-US")}
        </td>
        <td className="transfer-table__num" data-testid="count-updates">
          {c.updates.toLocaleString("en-US")}
        </td>
        <td className="transfer-table__num" data-testid="count-deletes">
          {c.deletes.toLocaleString("en-US")}
        </td>
        <td className="transfer-table__num">{c.same.toLocaleString("en-US")}</td>
      </tr>
    );
  }

  function pendingRows(): ReactNode {
    return requested.map((p, i) => {
      if (compare.results[i]) return null;
      const live = compare.progress[i];
      return (
        <tr
          key={`pending-${qualifiedName(p.source)}`}
          data-testid="data-table-row"
          data-table={p.source.name}
        >
          <th scope="row" className="transfer-table__name">
            {qualifiedName(p.source)}
          </th>
          <td>
            <span className="badge badge--info">
              {live ? `Comparing… ${plural(live.chunks, "chunk", "chunks")}` : "Waiting"}
            </span>
          </td>
          <td className="transfer-table__num">
            {live?.counts.sourceRows.toLocaleString("en-US") ?? ""}
          </td>
          <td className="transfer-table__num">
            {live?.counts.targetRows.toLocaleString("en-US") ?? ""}
          </td>
          <td className="transfer-table__num">
            {live?.counts.inserts.toLocaleString("en-US") ?? ""}
          </td>
          <td className="transfer-table__num">
            {live?.counts.updates.toLocaleString("en-US") ?? ""}
          </td>
          <td className="transfer-table__num">
            {live?.counts.deletes.toLocaleString("en-US") ?? ""}
          </td>
          <td className="transfer-table__num">{live?.counts.same.toLocaleString("en-US") ?? ""}</td>
        </tr>
      );
    });
  }

  function resultTable(): ReactNode {
    if (compare.status === "idle") return null;
    return (
      <table className="transfer-table" data-testid="data-results" aria-label="Compared tables">
        <thead>
          <tr>
            <th scope="col">Table</th>
            <th scope="col">Status</th>
            <th scope="col">Source rows</th>
            <th scope="col">Target rows</th>
            <th scope="col">Insert</th>
            <th scope="col">Update</th>
            <th scope="col">Delete</th>
            <th scope="col">Same</th>
          </tr>
        </thead>
        <tbody>
          {results.map(resultRow)}
          {comparing ? pendingRows() : null}
        </tbody>
      </table>
    );
  }

  function sampleCells(t: TableDataDiff, s: RowSample): ReactNode {
    const changed = new Set(s.changed ?? []);
    return t.columns.map((column, c) => {
      const src = s.source?.[c];
      const tgt = s.target?.[c];
      if (s.op === "update" && changed.has(column)) {
        return (
          <td key={column} className="sync-cell--changed" title={`${column} changes`}>
            <span className="sync-cell__old">{cell(tgt)}</span>
            <ArrowRight {...ICON_SM} />
            <span className="sync-cell__new">{cell(src)}</span>
          </td>
        );
      }
      return (
        <td key={column} className="transfer-table__mono">
          {cell(s.op === "delete" ? tgt : src)}
        </td>
      );
    });
  }

  function samples(): ReactNode {
    if (!active) return null;
    // `columns` lists the key first; key columns outside it (if any) get their own cells.
    const extraKey = active.key.flatMap((k, j) => (active.columns.includes(k) ? [] : [{ k, j }]));
    const rows = (active.samples ?? []).filter((s) => s.op === sampleOp);
    const counts = active.counts;
    const n: Record<RowOp, number> = {
      insert: counts.inserts,
      update: counts.updates,
      delete: counts.deletes,
    };
    return (
      <div className="sync-samples" data-testid="data-samples">
        <div className="sync-samples__head">
          <span className="sync-object__name">{qualifiedName(active.source)}</span>
          <span className="sync__meta">
            {`key ${active.key.join(", ") || "—"} · ${formatMs(active.elapsedMs)}${active.chunksSkipped > 0 ? ` · ${plural(active.chunksSkipped, "chunk", "chunks")} skipped by digest` : ""}`}
          </span>
        </div>
        {active.status.type === "refused" ? (
          <Note tone="warning" title="Not compared">
            {`${active.status.reason} Type key columns for this table and compare again.`}
          </Note>
        ) : null}
        <WarningList title="Warnings" items={active.warnings} />
        <div className="tabs" role="tablist" aria-label="Sample rows">
          {(["insert", "update", "delete"] as const).map((op) => (
            <button
              key={op}
              type="button"
              role="tab"
              id={`${id}-op-${op}`}
              className="tabs__tab"
              aria-selected={sampleOp === op}
              aria-controls={`${id}-samples`}
              onClick={() => setSampleOp(op)}
            >
              {`${OP_LABEL[op]} (${n[op].toLocaleString("en-US")})`}
            </button>
          ))}
        </div>
        <div role="tabpanel" id={`${id}-samples`} aria-labelledby={`${id}-op-${sampleOp}`}>
          {rows.length === 0 ? (
            <p className="sync__empty">No sample rows.</p>
          ) : (
            <div className="sync-samples__grid">
              <table className="transfer-table">
                <thead>
                  <tr>
                    {extraKey.map(({ k }) => (
                      <th key={`k-${k}`} scope="col">
                        {k}
                        <span className="visually-hidden"> (key)</span>
                      </th>
                    ))}
                    {active.columns.map((c) => (
                      <th key={c} scope="col">
                        {c}
                        {active.key.includes(c) ? (
                          <span className="visually-hidden"> (key)</span>
                        ) : null}
                      </th>
                    ))}
                  </tr>
                </thead>
                <tbody>
                  {rows.map((s, i) => (
                    // biome-ignore lint/suspicious/noArrayIndexKey: samples have no id of their own.
                    <tr key={i}>
                      {extraKey.map(({ k, j }) => (
                        <th key={`k-${k}`} scope="row" className="transfer-table__mono">
                          {cell(s.key[j])}
                        </th>
                      ))}
                      {sampleCells(active, s)}
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
          )}
        </div>
      </div>
    );
  }

  function scriptPanel(): ReactNode {
    if (!compareId) return null;
    const s = script.status === "ok" ? script.script : null;
    const any = totals.inserts + totals.updates + totals.deletes > 0;
    return (
      <div className="sync-data-script">
        <fieldset className="fieldset sync-ops">
          <legend>Sync</legend>
          <label className="checkbox">
            <input
              type="checkbox"
              checked={ops.inserts}
              disabled={busy}
              onChange={(e) => changeOps({ inserts: e.target.checked })}
            />
            {`Inserts (${totals.inserts.toLocaleString("en-US")})`}
          </label>
          <label className="checkbox">
            <input
              type="checkbox"
              checked={ops.updates}
              disabled={busy}
              onChange={(e) => changeOps({ updates: e.target.checked })}
            />
            {`Updates (${totals.updates.toLocaleString("en-US")})`}
          </label>
          <label className="checkbox">
            <input
              type="checkbox"
              checked={ops.deletes}
              disabled={busy}
              data-testid="data-deletes"
              onChange={(e) => changeOps({ deletes: e.target.checked })}
            />
            {`Deletes (${totals.deletes.toLocaleString("en-US")})`}
          </label>
          <button
            type="button"
            className="button"
            data-testid="data-preview"
            disabled={busy || !any || script.status === "loading"}
            onClick={() => void previewScript()}
          >
            {script.status === "loading" ? (
              <LoaderCircle {...ICON} className="spin" />
            ) : (
              <FileCode {...ICON} />
            )}
            Preview script
          </button>
          <button
            type="button"
            className="button button--primary"
            data-testid="data-apply"
            disabled={busy || !any || !(ops.inserts || ops.updates || ops.deletes)}
            onClick={() => {
              setAcknowledged(false);
              setConfirming(true);
            }}
          >
            <Play {...ICON} />
            Apply to target…
          </button>
        </fieldset>
        {script.status === "error" ? (
          <Note tone="error" title="Could not build the script">
            {script.message}
          </Note>
        ) : null}
        {s ? (
          <>
            <p className="sync__meta" data-testid="data-script-status">
              {s.truncated
                ? `Showing the first ${plural(s.statements.length, "statement", "statements")} of ${s.totalStatements.toLocaleString("en-US")}.`
                : plural(s.totalStatements, "statement", "statements")}
            </p>
            <WarningList title="Warnings" items={s.warnings} />
            <div className="sync-script__editor sync-script__editor--short">
              <SqlEditor
                initialValue=""
                value={scriptText(s)}
                language={language}
                editorRef={scriptEditor}
                readOnly
                ariaLabel="Data sync script"
                testId="data-script"
              />
            </div>
          </>
        ) : null}
      </div>
    );
  }

  function applyView(): ReactNode {
    if (apply.status === "idle") return null;
    if (applying) {
      return (
        <div className="sync-apply" data-testid="data-apply-progress">
          <p className="sync__running" role="status">
            <LoaderCircle {...ICON_SM} className="spin" />
            {apply.status === "recomparing"
              ? "Applied; comparing again…"
              : `Applying ${apply.done.toLocaleString("en-US")} of ${apply.total.toLocaleString("en-US")} statements…`}
          </p>
          <RunBar done={apply.done} total={apply.total || null} label="Apply progress" />
          <button
            type="button"
            className="button button--danger"
            disabled={apply.cancelling || apply.status === "recomparing"}
            onClick={cancelApply}
          >
            <CircleX {...ICON} />
            {apply.cancelling ? "Cancelling…" : "Cancel"}
          </button>
        </div>
      );
    }
    const r = apply.report;
    if (apply.status === "failed") {
      return (
        <Note tone="error" title="The apply failed" testId="data-result">
          {apply.error?.message}
          {r?.leftBehind ? ` ${r.leftBehind}` : ""}
        </Note>
      );
    }
    if (apply.status === "cancelled") {
      return (
        <Note tone="warning" title="Apply cancelled" testId="data-result">
          {r?.leftBehind || "Committed batches stay applied."}
        </Note>
      );
    }
    if (!r) return null;
    const changed = r.tables.reduce((n, t) => n + t.inserted + t.updated + t.deleted, 0);
    return r.inSync ? (
      <Note tone="ok" title="In sync" testId="data-result" role="status">
        {`${plural(changed, "row", "rows")} changed in ${formatMs(r.elapsedMs)}; a fresh compare finds no differences.`}
      </Note>
    ) : (
      <Note tone="warning" title="Differences remain" testId="data-result" role="status">
        {`${plural(changed, "row", "rows")} changed; a fresh compare still finds differences (deletes are only applied when chosen).`}
      </Note>
    );
  }

  return (
    <section className="sync" aria-label="Data sync" data-testid="data-sync">
      <div className="sync__setup">
        <ContainerPicker
          label="Source"
          sessions={sqlSessions}
          value={source}
          disabled={busy}
          testId="data-source"
          emptyHint="Open a connection to a SQL database first."
          onChange={(next) => {
            setSource(next);
            const nextEngine = sqlSessions.find((s) => s.id === next.sessionId)?.engine;
            const targetEngine = sqlSessions.find((s) => s.id === target.sessionId)?.engine;
            if (nextEngine && targetEngine && nextEngine !== targetEngine) {
              setTarget({ sessionId: "", database: "", schema: "", container: null });
            }
          }}
        />
        <span className="sync__arrow" aria-hidden="true">
          <ArrowRight {...ICON} />
        </span>
        <ContainerPicker
          label="Target"
          sessions={targetSessions}
          value={target}
          disabled={busy}
          testId="data-target"
          emptyHint={
            engine
              ? `Data sync needs two connections of the same engine (${ENGINE_INFO[engine].label}).`
              : undefined
          }
          onChange={setTarget}
        />
        <div className="sync__actions">
          {comparing ? (
            <button
              type="button"
              className="button button--danger"
              disabled={compare.cancelling || !compare.id}
              onClick={cancelCompare}
            >
              <CircleX {...ICON} />
              {compare.cancelling ? "Cancelling…" : "Cancel"}
            </button>
          ) : (
            <button
              type="button"
              className="button button--primary"
              data-testid="data-compare"
              disabled={!canCompare}
              title={sameContainer ? "Source and target are the same" : undefined}
              onClick={() => void runCompare()}
            >
              <GitCompareArrows {...ICON} />
              {compare.status === "idle" ? "Compare" : "Compare again"}
            </button>
          )}
        </div>
      </div>
      {sameContainer ? (
        <Note tone="warning" title="Source and target are the same" testId="data-same">
          Pick another database or schema as the target.
        </Note>
      ) : null}
      <details className="sync__options">
        <summary>Options</summary>
        <div className="sync__options-body">
          <div className="field">
            <label htmlFor={`${id}-chunk`}>Rows per chunk</label>
            <input
              id={`${id}-chunk`}
              type="number"
              min={100}
              step={1000}
              value={chunkRows}
              disabled={busy}
              onChange={(e) => setChunkRows(Math.max(100, Number(e.target.value) || 10_000))}
            />
          </div>
          <div className="field">
            <label htmlFor={`${id}-samples-n`}>Sample rows per table</label>
            <input
              id={`${id}-samples-n`}
              type="number"
              min={0}
              max={1000}
              value={sampleRows}
              disabled={busy}
              onChange={(e) =>
                setSampleRows(Math.min(1000, Math.max(0, Number(e.target.value) || 0)))
              }
            />
          </div>
          <label className="checkbox">
            <input
              type="checkbox"
              checked={useChunkHash}
              disabled={busy}
              onChange={(e) => setUseChunkHash(e.target.checked)}
            />
            Skip identical chunks by digest (where the engine can)
          </label>
        </div>
      </details>
      {compare.status === "idle" ? (
        pairTable()
      ) : (
        <details className="sync__pairs-again">
          <summary>{`Tables (${chosen.length} of ${pairs.length} paired)`}</summary>
          {pairTable()}
        </details>
      )}
      {compare.status === "failed" ? (
        <Note tone="error" title="The compare failed" testId="data-error">
          {compare.error?.message}
        </Note>
      ) : null}
      {compare.status === "cancelled" ? (
        <Note tone="warning" title="Compare cancelled">
          Tables compared before the cancel are listed.
        </Note>
      ) : null}
      {stale && compare.status === "finished" ? (
        <Note tone="warning" title="The source or target changed" testId="data-stale">
          {`The results below compare against ${comparedTarget}. Compare again before building a script or applying.`}
        </Note>
      ) : null}
      {applyView()}
      {compare.status !== "idle" ? (
        <div className="sync__results">
          {comparing ? (
            <RunBar
              done={results.length}
              total={requested.length || null}
              label="Compare progress"
            />
          ) : (
            <p className="sync__meta" data-testid="data-totals">
              {`${plural(totals.inserts, "insert", "inserts")} · ${plural(totals.updates, "update", "updates")} · ${plural(totals.deletes, "delete", "deletes")}${compare.result ? ` · ${formatMs(compare.result.elapsedMs)}` : ""}`}
            </p>
          )}
          {resultTable()}
          {samples()}
          {scriptPanel()}
        </div>
      ) : tables.status === "idle" ? (
        <div className="empty-state">
          <span className="empty-state__icon" aria-hidden="true">
            <GitCompareArrows {...ICON_LG} />
          </span>
          <p className="empty-state__title">Compare the rows of two databases or schemas</p>
          <p className="empty-state__hint">
            Pick a source and a target of the same engine; tables pair up by name.
          </p>
        </div>
      ) : null}
      {confirming && compareId ? (
        <Modal
          title="Apply the data changes?"
          onClose={() => setConfirming(false)}
          className="modal--transfer-narrow sync-confirm"
          footer={
            <>
              <button type="button" className="button" onClick={() => setConfirming(false)}>
                Cancel
              </button>
              <button
                type="button"
                className={`button ${ops.deletes ? "button--danger" : "button--primary"}`}
                data-testid="data-confirm"
                disabled={ops.deletes && !acknowledged}
                onClick={() => void runApply()}
              >
                {ops.deletes ? <TriangleAlert {...ICON} /> : <Check {...ICON} />}
                {ops.deletes ? "Apply, including deletes" : "Apply"}
              </button>
            </>
          }
        >
          <p>
            {[
              ops.inserts ? plural(totals.inserts, "insert", "inserts") : null,
              ops.updates ? plural(totals.updates, "update", "updates") : null,
              ops.deletes ? plural(totals.deletes, "delete", "deletes") : null,
            ]
              .filter(Boolean)
              .join(", ")}{" "}
            will run on <strong data-testid="data-confirm-target">{comparedTarget}</strong>, in
            transactions of up to 1,000 statements. Each statement must change exactly one row, else
            its batch rolls back and the apply stops.
          </p>
          {ops.deletes ? (
            <>
              <Note tone="error" title="Deletes remove target rows">
                {`${plural(totals.deletes, "row", "rows")} that exist only in the target will be deleted.`}
              </Note>
              <Acknowledge
                checked={acknowledged}
                onChange={setAcknowledged}
                testId="data-acknowledge"
              >
                Delete the target rows that are not in the source.
              </Acknowledge>
            </>
          ) : null}
        </Modal>
      ) : null}
    </section>
  );
}
