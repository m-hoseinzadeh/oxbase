/**
 * The data transfer wizard (docs/m8-contracts.md §4, §8.4): source (a connection's tables and
 * views, or a query) → target (a connection's database or schema, any SQL engine) → objects
 * (target names, row filters) → mapping (per column: source type, target type, a lossiness
 * badge with the mapper's reason, overrides) → options → review (the dry run: DDL, load order,
 * warnings, destructive confirmation) → run (per-table progress, row errors, cancel) → report.
 * Nothing is written before Start.
 */
import {
  ArrowLeft,
  ArrowRight,
  Check,
  CircleX,
  LoaderCircle,
  Play,
  Search,
  TriangleAlert,
} from "lucide-react";
import { type ReactNode, useEffect, useId, useMemo, useReducer, useRef, useState } from "react";
import {
  type DataType,
  type ObjectRef,
  type SourceObject,
  type TablePlan,
  type TransferOptions,
  type TransferPlan,
  type TransferRequest,
  transferCancel,
  transferPlan,
  transferRun,
} from "../../api";
import { ICON, ICON_SM } from "../../components/icons";
import { Modal } from "../../components/Modal";
import { cancelRequest, newRequestId, toCommandError } from "../../connections/api";
import { useConnections } from "../../connections/store";
import { languageFor } from "../editor/language";
import { SqlEditor, type SqlEditorHandle } from "../editor/SqlEditor";
import { formatMs } from "../format";
import { engineFamily } from "../nosql/engines";
import { formatBytes, typeChoices, typeKey, typeLabel } from "../transfer/model";
import { ContainerPicker, type SideValue, sideFromInit } from "./ContainerPicker";
import { listTables } from "./catalog";
import {
  buildSpec,
  containerLabel,
  DEFAULT_TRANSFER_OPTIONS,
  ended,
  IDLE_TRANSFER,
  itemsProblem,
  type OverrideEdit,
  type OverrideMap,
  overrideKey,
  planBlockers,
  planDdl,
  plural,
  qualifiedName,
  refKey,
  sameRef,
  type TransferItem,
  transferReducer,
  transferTotals,
} from "./model";
import { PairReportView } from "./PairReportView";
import { Acknowledge, FidelityBadge, Note, RunBar, WarningList } from "./parts";
import { setSyncBusy, type TransferInit } from "./store";

type Step = "source" | "target" | "objects" | "mapping" | "options" | "review" | "run";

const STEPS: { id: Step; label: string }[] = [
  { id: "source", label: "Source" },
  { id: "target", label: "Target" },
  { id: "objects", label: "Objects" },
  { id: "mapping", label: "Mapping" },
  { id: "options", label: "Options" },
  { id: "review", label: "Review" },
  { id: "run", label: "Run" },
];

type Listing =
  | { status: "idle" }
  | { status: "loading" }
  | { status: "ok"; refs: ObjectRef[] }
  | { status: "error"; message: string };

type PlanState =
  | { status: "idle" }
  | { status: "loading"; key: string }
  | { status: "ok"; key: string; plan: TransferPlan }
  | { status: "error"; key: string; message: string };

const MODE_LABEL: Record<NonNullable<TransferOptions["mode"]>, string> = {
  create: "Create new tables (fail if one exists)",
  replace: "Replace existing tables (drop and create)",
  append: "Append to existing tables",
  truncate: "Empty existing tables, then load",
};

const STATUS_TEXT = {
  done: "Done",
  failed: "Failed",
  cancelled: "Cancelled",
  not_run: "Not run",
} as const;

/** The override key of a planned object (what `itemKey` gives for the wizard item it came from). */
function objectKey(o: SourceObject): string {
  if (o.type === "table") return refKey(o.table);
  if (o.type === "view") return refKey(o.view);
  return "query";
}

export function TransferWizard({ init, onClose }: { init: TransferInit; onClose: () => void }) {
  const { sessions } = useConnections();
  const id = useId();
  const sqlSessions = sessions.filter((s) => engineFamily(s.engine) === "sql");
  const [step, setStep] = useState<Step>("source");
  const [source, setSource] = useState<SideValue>(() =>
    sideFromInit(init.source, sqlSessions[0]?.id ?? ""),
  );
  const [target, setTarget] = useState<SideValue>(() => sideFromInit(undefined));
  const [sourceKind, setSourceKind] = useState<"objects" | "query">("objects");
  const [listing, setListing] = useState<Listing>({ status: "idle" });
  const [filter, setFilter] = useState("");
  const [checked, setChecked] = useState<string[]>(() => (init.objects ?? []).map(refKey));
  const [edits, setEdits] = useState<Record<string, { targetName: string; where: string }>>({});
  const [query, setQuery] = useState({ sql: "", targetName: "query_result" });
  const [options, setOptions] = useState<Required<TransferOptions>>(DEFAULT_TRANSFER_OPTIONS);
  const [overrides, setOverrides] = useState<OverrideMap>({});
  const [plan, setPlan] = useState<PlanState>({ status: "idle" });
  // The request a destructive acknowledgement was given for: it holds for that request only.
  const [confirmedKey, setConfirmedKey] = useState("");
  const [run, dispatch] = useReducer(transferReducer, IDLE_TRANSFER);
  const [mappingTable, setMappingTable] = useState(0);
  const alive = useRef(true);
  const runId = useRef<string | null>(null);
  const ddlEditor = useRef<SqlEditorHandle | null>(null);

  const running = run.status === "starting" || run.status === "running";
  const sourceSession = sqlSessions.find((s) => s.id === source.sessionId) ?? null;
  const targetSession = sqlSessions.find((s) => s.id === target.sessionId) ?? null;
  const sourceKey = source.container ? JSON.stringify(source.container) : "";

  useEffect(() => {
    alive.current = true;
    return () => {
      alive.current = false;
      if (runId.current) void transferCancel(runId.current).catch(() => {});
    };
  }, []);

  useEffect(() => {
    setSyncBusy(running);
    return () => setSyncBusy(false);
  }, [running]);

  // The tables and views of the source container.
  useEffect(() => {
    if (!sourceKey) {
      setListing({ status: "idle" });
      return;
    }
    let current = true;
    setListing({ status: "loading" });
    listTables(source.sessionId, JSON.parse(sourceKey) as ObjectRef, true, newRequestId()).then(
      (refs) => {
        if (!current) return;
        setListing({ status: "ok", refs });
        // Objects of another database or schema do not stay selected.
        const known = new Set(refs.map(refKey));
        setChecked((all) =>
          all.every((k) => known.has(k)) ? all : all.filter((k) => known.has(k)),
        );
      },
      (error) => current && setListing({ status: "error", message: toCommandError(error).message }),
    );
    return () => {
      current = false;
    };
  }, [source.sessionId, sourceKey]);

  const refs = listing.status === "ok" ? listing.refs : [];
  const items: TransferItem[] = useMemo(() => {
    if (sourceKind === "query") {
      return [{ sql: query.sql, targetName: query.targetName, where: "" }];
    }
    const byKey = new Map(refs.map((r) => [refKey(r), r]));
    return checked.flatMap((key) => {
      const ref = byKey.get(key);
      if (!ref) return [];
      const edit = edits[key];
      return [{ ref, targetName: edit?.targetName ?? ref.name, where: edit?.where ?? "" }];
    });
  }, [sourceKind, query, refs, checked, edits]);

  const request: TransferRequest | null =
    source.container && target.container && items.length > 0
      ? {
          sourceSession: source.sessionId,
          targetSession: target.sessionId,
          spec: buildSpec(items, target.container, options, overrides),
        }
      : null;
  const requestKey = request ? JSON.stringify(request) : "";
  // Writing into the container the rows are read from: only "create" (which never touches an
  // existing table) is safe; replace or truncate would empty a source table before reading it.
  const sameAsSource =
    sourceKind === "objects" &&
    source.sessionId !== "" &&
    target.sessionId === source.sessionId &&
    sameRef(target.container, source.container);
  const modeAllowed = !sameAsSource || options.mode === "create";
  // Any change to the request drops the acknowledgement (also when it changes back).
  if (confirmedKey !== "" && confirmedKey !== requestKey) setConfirmedKey("");
  const confirmed = confirmedKey !== "" && confirmedKey === requestKey;
  const wantsPlan = step === "mapping" || step === "options" || step === "review";

  // The plan (dry run) follows the request while the mapping, options and review steps show.
  useEffect(() => {
    if (!wantsPlan || !requestKey) return;
    let current = true;
    const requestId = newRequestId();
    setPlan((p) =>
      p.status !== "idle" && p.key === requestKey ? p : { status: "loading", key: requestKey },
    );
    const timer = setTimeout(() => {
      transferPlan(JSON.parse(requestKey) as TransferRequest, requestId).then(
        (next) => current && setPlan({ status: "ok", key: requestKey, plan: next }),
        (error) =>
          current &&
          setPlan({ status: "error", key: requestKey, message: toCommandError(error).message }),
      );
    }, 250);
    return () => {
      current = false;
      clearTimeout(timer);
      void cancelRequest(requestId).catch(() => {});
    };
  }, [wantsPlan, requestKey]);

  const currentPlan = plan.status === "ok" ? plan.plan : null;
  const planIsCurrent = plan.status === "ok" && plan.key === requestKey;
  // The override key of each planned object, from the request the plan was made for (a plan
  // still on screen while the next one loads may number the objects differently).
  const planKey = plan.status === "ok" ? plan.key : "";
  const planKeys = useMemo(
    () =>
      planKey
        ? (JSON.parse(planKey) as TransferRequest).spec.objects.map(objectKey)
        : ([] as string[]),
    [planKey],
  );

  // The same container on both sides only allows "create".
  useEffect(() => {
    if (sameAsSource && options.mode !== "create") setOptions((o) => ({ ...o, mode: "create" }));
  }, [sameAsSource, options.mode]);

  async function start() {
    if (!request || !currentPlan || !planIsCurrent || !modeAllowed) return;
    setStep("run");
    dispatch({ type: "start" });
    try {
      const opId = await transferRun(
        { ...request, confirmDestructive: currentPlan.destructive && confirmed },
        (event) => {
          if (!alive.current) return;
          dispatch({ type: "event", event });
          if (ended(event.type)) runId.current = null;
        },
      );
      runId.current = opId;
    } catch (error) {
      if (alive.current) dispatch({ type: "startFailed", error: toCommandError(error) });
    }
  }

  function cancel() {
    const opId = runId.current ?? run.id;
    if (!opId) return;
    dispatch({ type: "cancelling" });
    void transferCancel(opId).catch(() => {});
  }

  function close() {
    if (running) return;
    onClose();
  }

  function overrideFor(table: TablePlan, column: string): OverrideEdit {
    const item = planKeys[table.object];
    return item === undefined ? {} : (overrides[overrideKey(item, column)] ?? {});
  }

  function setOverride(table: TablePlan, column: string, patch: OverrideEdit) {
    const item = planKeys[table.object];
    if (item === undefined) return;
    const key = overrideKey(item, column);
    setOverrides((all) => ({ ...all, [key]: { ...(all[key] ?? {}), ...patch } }));
  }

  // --- steps ------------------------------------------------------------------------------

  function sourceStep(): ReactNode {
    const shown = refs.filter((r) => r.name.toLowerCase().includes(filter.trim().toLowerCase()));
    return (
      <div className="transfer-step">
        <ContainerPicker
          label="Source"
          sessions={sqlSessions}
          value={source}
          testId="transfer-source"
          emptyHint="Open a connection to a SQL database first (MongoDB and Redis are not transfer sources yet)."
          onChange={(next) => {
            // The picker reports no container while it lists again (also when this step shows
            // again), so the selection is pruned when the listing arrives, not cleared here.
            if (next.sessionId !== source.sessionId) setChecked([]);
            setSource(next);
          }}
        />
        <div className="segmented" role="radiogroup" aria-label="What to transfer">
          {(["objects", "query"] as const).map((kind) => (
            <label key={kind} className="segmented__item transfer-radio">
              <input
                type="radio"
                name={`${id}-kind`}
                value={kind}
                checked={sourceKind === kind}
                onChange={() => setSourceKind(kind)}
              />
              {kind === "objects" ? "Tables and views" : "A query"}
            </label>
          ))}
        </div>
        {sourceKind === "query" ? (
          <div className="field">
            <label htmlFor={`${id}-sql`}>Query</label>
            <textarea
              id={`${id}-sql`}
              className="sync-query"
              rows={6}
              spellCheck={false}
              value={query.sql}
              placeholder="SELECT … FROM …"
              onChange={(e) => setQuery({ ...query, sql: e.target.value })}
            />
            <p className="field__hint">Its result lands in one new target table.</p>
          </div>
        ) : listing.status === "loading" ? (
          <p className="transfer-step__status" role="status">
            <LoaderCircle {...ICON_SM} className="spin" />
            Listing tables and views…
          </p>
        ) : listing.status === "error" ? (
          <Note tone="error" title="Could not list the tables">
            {listing.message}
          </Note>
        ) : listing.status === "ok" ? (
          <>
            <div className="sync-filter">
              <Search {...ICON_SM} />
              <input
                type="search"
                aria-label="Filter tables by name"
                placeholder="Filter by name"
                value={filter}
                onChange={(e) => setFilter(e.target.value)}
              />
              <button
                type="button"
                className="button button--small button--ghost"
                onClick={() => setChecked([...new Set([...checked, ...shown.map(refKey)])])}
              >
                Select shown
              </button>
              <button
                type="button"
                className="button button--small button--ghost"
                onClick={() => {
                  const off = new Set(shown.map(refKey));
                  setChecked(checked.filter((k) => !off.has(k)));
                }}
              >
                Clear shown
              </button>
            </div>
            <ul
              className="sync-checklist"
              aria-label="Tables and views"
              data-testid="transfer-objects"
            >
              {shown.map((ref) => {
                const key = refKey(ref);
                return (
                  <li key={key}>
                    <label className="checkbox">
                      <input
                        type="checkbox"
                        checked={checked.includes(key)}
                        onChange={(e) =>
                          setChecked(
                            e.target.checked ? [...checked, key] : checked.filter((k) => k !== key),
                          )
                        }
                      />
                      <span className="transfer-table__name">{qualifiedName(ref)}</span>
                      {ref.kind === "view" ? <span className="badge badge--info">view</span> : null}
                    </label>
                  </li>
                );
              })}
              {shown.length === 0 ? <li className="sync__empty">No tables match.</li> : null}
            </ul>
            <p className="transfer-step__status" role="status">
              {`${plural(checked.length, "object", "objects")} selected`}
            </p>
          </>
        ) : (
          <p className="sync__empty">Choose a database or schema to list its tables.</p>
        )}
      </div>
    );
  }

  function targetStep(): ReactNode {
    return (
      <div className="transfer-step">
        <ContainerPicker
          label="Target"
          sessions={sqlSessions}
          value={target}
          testId="transfer-target"
          onChange={setTarget}
        />
        {sameAsSource ? (
          <Note tone="warning" title="Source and target are the same" testId="transfer-same-target">
            Tables can only be created under new names here: replacing, emptying or appending to an
            existing table would change the rows being read. Give each object a new target name on
            the next step.
          </Note>
        ) : null}
        <p className="field__hint">
          Any SQL engine. The database or schema must exist; tables are created in it as the mode
          (next steps) says. Only tables, their keys and indexes travel across engines.
        </p>
        {sourceSession && targetSession && source.container && target.container ? (
          <Note tone="info">
            {`${sourceSession.profile_name} · ${containerLabel(source.container)} → ${targetSession.profile_name} · ${containerLabel(target.container)}`}
          </Note>
        ) : null}
        {sourceSession && targetSession ? (
          <PairReportView sourceSession={sourceSession.id} targetSession={targetSession.id} />
        ) : null}
      </div>
    );
  }

  function objectsStep(): ReactNode {
    if (sourceKind === "query") {
      return (
        <div className="transfer-step">
          <div className="field">
            <label htmlFor={`${id}-query-target`}>Target table</label>
            <input
              id={`${id}-query-target`}
              type="text"
              value={query.targetName}
              onChange={(e) => setQuery({ ...query, targetName: e.target.value })}
            />
          </div>
          {modeField()}
        </div>
      );
    }
    return (
      <div className="transfer-step">
        {modeField()}
        <table className="transfer-table transfer-mapping" aria-label="Objects">
          <thead>
            <tr>
              <th scope="col">Source</th>
              <th scope="col">Target table</th>
              <th scope="col">Only rows where (SQL condition)</th>
            </tr>
          </thead>
          <tbody>
            {items.map((item) => {
              const ref = item.ref as ObjectRef;
              const key = refKey(ref);
              const edit = edits[key] ?? { targetName: ref.name, where: "" };
              return (
                <tr key={key}>
                  <th scope="row" className="transfer-table__name">
                    {qualifiedName(ref)}
                  </th>
                  <td>
                    <input
                      type="text"
                      aria-label={`Target table of ${ref.name}`}
                      value={edit.targetName}
                      onChange={(e) =>
                        setEdits({ ...edits, [key]: { ...edit, targetName: e.target.value } })
                      }
                    />
                  </td>
                  <td>
                    <input
                      type="text"
                      aria-label={`Row filter of ${ref.name}`}
                      placeholder="every row"
                      value={edit.where}
                      onChange={(e) =>
                        setEdits({ ...edits, [key]: { ...edit, where: e.target.value } })
                      }
                    />
                  </td>
                </tr>
              );
            })}
          </tbody>
        </table>
      </div>
    );
  }

  function modeField(): ReactNode {
    return (
      <div className="field">
        <label htmlFor={`${id}-mode`}>When a target table exists</label>
        <select
          id={`${id}-mode`}
          value={options.mode}
          onChange={(e) =>
            setOptions({ ...options, mode: e.target.value as Required<TransferOptions>["mode"] })
          }
        >
          {(Object.keys(MODE_LABEL) as (keyof typeof MODE_LABEL)[]).map((m) => (
            <option key={m} value={m} disabled={sameAsSource && m !== "create"}>
              {MODE_LABEL[m]}
            </option>
          ))}
        </select>
        {sameAsSource ? (
          <p className="field__hint">
            The target is the source database or schema: only new tables can be created.
          </p>
        ) : null}
        {options.mode === "replace" || options.mode === "truncate" ? (
          <p className="field__hint sync-danger-hint">
            <TriangleAlert {...ICON_SM} />
            Destructive: existing target rows are lost. You confirm it on the review step.
          </p>
        ) : null}
      </div>
    );
  }

  function planStatus(): ReactNode {
    if (plan.status === "loading" || (plan.status === "ok" && !planIsCurrent)) {
      return (
        <p className="transfer-step__status" role="status" data-testid="transfer-plan-status">
          <LoaderCircle {...ICON_SM} className="spin" />
          Planning (dry run, nothing is written)…
        </p>
      );
    }
    if (plan.status === "error") {
      return (
        <Note tone="error" title="Could not plan the transfer" testId="transfer-plan-error">
          {plan.message}
        </Note>
      );
    }
    return null;
  }

  function columnTypeCell(table: TablePlan, c: TablePlan["columns"][number]): ReactNode {
    const edit = overrideFor(table, c.source);
    const choices: DataType[] = typeChoices(c.mapping.dataType);
    return (
      <div className="sync-type">
        <select
          aria-label={`Target type of ${c.source}`}
          value={typeKey(edit.dataType ?? c.mapping.dataType)}
          disabled={c.skipped}
          onChange={(e) => {
            const picked = choices.find((t) => typeKey(t) === e.target.value);
            if (picked) setOverride(table, c.source, { dataType: picked, nativeType: "" });
          }}
        >
          {choices.map((t) => (
            <option key={typeKey(t)} value={typeKey(t)}>
              {typeLabel(t)}
            </option>
          ))}
        </select>
        <input
          type="text"
          aria-label={`Native target type of ${c.source}`}
          placeholder={c.mapping.nativeType || "derived"}
          value={edit.nativeType ?? ""}
          disabled={c.skipped}
          onChange={(e) => setOverride(table, c.source, { nativeType: e.target.value })}
        />
      </div>
    );
  }

  function mappingStep(): ReactNode {
    const p = currentPlan;
    const tables = p?.tables ?? [];
    const index = Math.min(mappingTable, Math.max(0, tables.length - 1));
    const table = tables[index];
    return (
      <div className="transfer-step transfer-step--split">
        <div className="transfer-step__side">
          <ul className="sync-checklist" aria-label="Tables">
            {tables.map((t, i) => (
              <li key={t.object}>
                <button
                  type="button"
                  className={`sync-link sync-table-pick${i === index ? " sync-table-pick--active" : ""}`}
                  aria-current={i === index ? "true" : undefined}
                  onClick={() => setMappingTable(i)}
                >
                  <span className="transfer-table__name">{qualifiedName(t.target)}</span>
                  <FidelityBadge fidelity={t.fidelity} />
                </button>
              </li>
            ))}
          </ul>
        </div>
        <div className="transfer-step__main">
          {planStatus()}
          {p ? (
            <div className="sync__summary">
              <span>{`${p.sourceEngine} → ${p.targetEngine}`}</span>
              <FidelityBadge fidelity={p.fidelity} />
            </div>
          ) : null}
          {table ? (
            <>
              <WarningList title={`About ${qualifiedName(table.target)}`} items={table.warnings} />
              <div className="transfer-mapping">
                <table
                  className="transfer-table"
                  data-testid="transfer-mapping"
                  aria-label="Column mapping"
                >
                  <thead>
                    <tr>
                      <th scope="col">Copy</th>
                      <th scope="col">Source column</th>
                      <th scope="col">Source type</th>
                      <th scope="col">Target column</th>
                      <th scope="col">Target type</th>
                      <th scope="col">Fidelity</th>
                    </tr>
                  </thead>
                  <tbody>
                    {table.columns.map((c) => {
                      const edit = overrideFor(table, c.source);
                      return (
                        <tr
                          key={c.source}
                          data-testid="mapping-row"
                          data-column={c.source}
                          className={c.skipped ? "sync-row--skipped" : undefined}
                        >
                          <td>
                            <input
                              type="checkbox"
                              aria-label={`Copy ${c.source}`}
                              checked={!c.skipped}
                              onChange={(e) =>
                                setOverride(table, c.source, { skip: !e.target.checked })
                              }
                            />
                          </td>
                          <th scope="row" className="transfer-table__name">
                            {c.source}
                          </th>
                          <td className="transfer-table__type">
                            {c.sourceNative || typeLabel(c.sourceType)}
                          </td>
                          <td>
                            <input
                              type="text"
                              aria-label={`Target column of ${c.source}`}
                              value={edit.targetName ?? c.target}
                              disabled={c.skipped}
                              onChange={(e) =>
                                setOverride(table, c.source, { targetName: e.target.value })
                              }
                            />
                          </td>
                          <td>{columnTypeCell(table, c)}</td>
                          <td>
                            <FidelityBadge
                              fidelity={c.mapping.fidelity}
                              reason={c.mapping.reason}
                            />
                            {c.overridden ? <span className="sync__meta"> overridden</span> : null}
                          </td>
                        </tr>
                      );
                    })}
                  </tbody>
                </table>
              </div>
            </>
          ) : p ? (
            <p className="sync__empty">No tables in the plan.</p>
          ) : null}
        </div>
      </div>
    );
  }

  function optionsStep(): ReactNode {
    const set = (patch: Partial<Required<TransferOptions>>) => setOptions({ ...options, ...patch });
    return (
      <div className="transfer-step">
        <div className="transfer-options">
          <fieldset className="fieldset transfer-group">
            <legend>Writing</legend>
            {modeField()}
            <div className="field">
              <label htmlFor={`${id}-batch`}>Rows per batch</label>
              <input
                id={`${id}-batch`}
                type="number"
                min={1}
                value={options.batchRows}
                onChange={(e) => set({ batchRows: Math.max(1, Number(e.target.value) || 1) })}
              />
            </div>
            <div className="field">
              <label htmlFor={`${id}-tx`}>Commit</label>
              <select
                id={`${id}-tx`}
                value={options.transaction}
                onChange={(e) => set({ transaction: e.target.value as "per_table" | "per_batch" })}
              >
                <option value="per_table">Once per table (all or nothing)</option>
                <option value="per_batch">After every batch</option>
              </select>
            </div>
          </fieldset>
          <fieldset className="fieldset transfer-group">
            <legend>Rows that do not fit</legend>
            <div className="field">
              <label htmlFor={`${id}-errors`}>On a bad row</label>
              <select
                id={`${id}-errors`}
                value={options.errors}
                onChange={(e) => set({ errors: e.target.value as "stop" | "continue" })}
              >
                <option value="stop">Stop the table</option>
                <option value="continue">Skip the row and report it</option>
              </select>
            </div>
            <div className="field">
              <label htmlFor={`${id}-max-errors`}>Rows reported at most</label>
              <input
                id={`${id}-max-errors`}
                type="number"
                min={0}
                value={options.maxReportedErrors}
                onChange={(e) =>
                  set({ maxReportedErrors: Math.max(0, Number(e.target.value) || 0) })
                }
              />
            </div>
          </fieldset>
          <fieldset className="fieldset transfer-group">
            <legend>Keys and identity</legend>
            <div className="field">
              <label htmlFor={`${id}-identity`}>Identity values</label>
              <select
                id={`${id}-identity`}
                value={options.identity}
                onChange={(e) => set({ identity: e.target.value as "keep" | "generate" })}
              >
                <option value="keep">Keep the source values</option>
                <option value="generate">Let the target generate them</option>
              </select>
            </div>
            <div className="field">
              <label htmlFor={`${id}-fk`}>Foreign key checks while loading</label>
              <select
                id={`${id}-fk`}
                value={options.fkChecks}
                onChange={(e) => set({ fkChecks: e.target.value as "keep" | "disable" })}
              >
                <option value="keep">Keep them on</option>
                <option value="disable">Turn them off where the engine can</option>
              </select>
            </div>
            <label className="checkbox">
              <input
                type="checkbox"
                checked={options.createIndexes}
                onChange={(e) => set({ createIndexes: e.target.checked })}
              />
              Create indexes
            </label>
            <label className="checkbox">
              <input
                type="checkbox"
                checked={options.createForeignKeys}
                onChange={(e) => set({ createForeignKeys: e.target.checked })}
              />
              Create foreign keys (after the load)
            </label>
          </fieldset>
        </div>
      </div>
    );
  }

  function reviewStep(): ReactNode {
    const p = currentPlan;
    if (!p) return <div className="transfer-step">{planStatus()}</div>;
    const blockers = planBlockers(p);
    const overwritten = p.tables.filter(
      (t) => t.exists && (t.mode === "replace" || t.mode === "truncate"),
    );
    const order = p.order.length > 0 ? p.order : p.tables.map((_, i) => i);
    return (
      <div className="transfer-step transfer-step--split">
        <div className="transfer-step__side">
          <dl className="transfer-summary" data-testid="transfer-review">
            <dt>From</dt>
            <dd>{`${sourceSession?.profile_name ?? ""} · ${containerLabel(source.container)}`}</dd>
            <dt>To</dt>
            <dd>{`${targetSession?.profile_name ?? ""} · ${containerLabel(target.container)}`}</dd>
            <dt>Tables</dt>
            <dd className="transfer-summary__num">{p.tables.length}</dd>
            <dt>Fidelity</dt>
            <dd>
              <FidelityBadge fidelity={p.fidelity} />
            </dd>
            <dt>Mode</dt>
            <dd>{options.mode}</dd>
          </dl>
          <p className="transfer-label">Load order</p>
          <ol className="sync-order" data-testid="transfer-order">
            {order.map((i) => {
              const t = p.tables[i];
              return t ? <li key={i}>{qualifiedName(t.target)}</li> : null;
            })}
          </ol>
        </div>
        <div className="transfer-step__main">
          {planStatus()}
          {blockers.length > 0 ? (
            <Note tone="error" title="The transfer cannot start" testId="transfer-blocked">
              <ul className="banner__list">
                {blockers.map((b, i) => (
                  // biome-ignore lint/suspicious/noArrayIndexKey: messages can repeat.
                  <li key={i}>{b}</li>
                ))}
              </ul>
            </Note>
          ) : null}
          {(p.cycles ?? []).length > 0 ? (
            <Note tone="warning" title="Foreign key cycles">
              {(p.cycles ?? [])
                .map((c) => c.map((i) => p.tables[i]?.target.name ?? "?").join(" ↔ "))
                .join("; ")}
              . Their foreign keys are added after every table is loaded.
            </Note>
          ) : null}
          {!p.transactionalDdl ? (
            <Note tone="warning" title="DDL is not transactional on the target">
              Each CREATE commits on its own; a failed table is dropped again where the run created
              it.
            </Note>
          ) : null}
          <WarningList title="Warnings" items={p.warnings} />
          {p.destructive ? (
            <>
              <Note tone="error" title="This transfer overwrites target data">
                {options.mode === "replace"
                  ? "Existing target tables are dropped and created again:"
                  : "Existing target tables are emptied before the load:"}
                <ul className="banner__list" data-testid="transfer-destructive-tables">
                  {overwritten.map((t) => (
                    <li
                      key={t.object}
                    >{`${targetSession?.profile_name ?? ""} · ${qualifiedName(t.target)}`}</li>
                  ))}
                </ul>
              </Note>
              <Acknowledge
                checked={confirmed}
                onChange={(on) => setConfirmedKey(on ? requestKey : "")}
                testId="transfer-acknowledge"
              >
                I understand that the target rows are lost.
              </Acknowledge>
            </>
          ) : null}
          <div className="sync-script__editor">
            <SqlEditor
              initialValue=""
              value={planDdl(p)}
              language={languageFor(p.targetEngine)}
              editorRef={ddlEditor}
              readOnly
              ariaLabel="Target DDL (dry run)"
              testId="transfer-ddl"
            />
          </div>
        </div>
      </div>
    );
  }

  function runStep(): ReactNode {
    const p = run.plan ?? currentPlan;
    const totals = transferTotals({ ...run, plan: p });
    const order = p ? (p.order.length > 0 ? p.order : p.tables.map((_, i) => i)) : [];
    const finished = ended(run.status);
    return (
      <div className="transfer-step">
        <div className="transfer-step__status" role="status" data-testid="transfer-status">
          {run.status === "starting" ? (
            <>
              <LoaderCircle {...ICON_SM} className="spin" />
              Starting…
            </>
          ) : run.status === "running" ? (
            <>
              <LoaderCircle {...ICON_SM} className="spin" />
              {`Copying · ${totals.tablesDone} of ${p?.tables.length ?? 0} tables · ${totals.rowsWritten.toLocaleString("en-US")} rows`}
            </>
          ) : run.status === "finished" ? (
            `Finished · ${totals.rowsWritten.toLocaleString("en-US")} rows in ${formatMs(run.report?.elapsedMs ?? 0)}`
          ) : run.status === "failed" ? (
            "Failed"
          ) : run.status === "cancelled" ? (
            "Cancelled"
          ) : null}
        </div>
        {!finished ? (
          <RunBar done={totals.rowsWritten} total={totals.rowsTotal} label="Transfer progress" />
        ) : null}
        {run.status === "failed" && run.error ? (
          <Note tone="error" title="The transfer failed" testId="transfer-error">
            {run.error.message}
          </Note>
        ) : null}
        {run.status === "finished" ? (
          <Note tone="ok" title="Transfer finished" testId="transfer-result" role="status">
            {`${plural(totals.tablesDone, "table", "tables")}, ${plural(totals.rowsWritten, "row", "rows")}${run.report?.foreignKeysAdded ? `, ${plural(run.report.foreignKeysAdded, "foreign key", "foreign keys")} added` : ""}${totals.errors > 0 ? `, ${plural(totals.errors, "row", "rows")} skipped` : ""}.`}
          </Note>
        ) : null}
        {run.status === "cancelled" ? (
          <Note tone="warning" title="Transfer cancelled" testId="transfer-result">
            What each table left behind is listed below.
          </Note>
        ) : null}
        <WarningList title="Warnings" items={run.report?.warnings} />
        <table className="transfer-table" data-testid="transfer-tables" aria-label="Tables">
          <thead>
            <tr>
              <th scope="col">Table</th>
              <th scope="col">Status</th>
              <th scope="col">Rows written</th>
              <th scope="col">Bytes</th>
              <th scope="col">Errors</th>
              <th scope="col">Time</th>
            </tr>
          </thead>
          <tbody>
            {order.map((i) => {
              const t = p?.tables[i];
              if (!t) return null;
              const progress = run.progress[i];
              const report = run.reports[i];
              const status = report
                ? STATUS_TEXT[report.status]
                : run.current === i
                  ? "Copying…"
                  : finished
                    ? "Not run"
                    : "Waiting";
              const total = progress?.totalRows ?? t.estimatedRows;
              return (
                <tr key={i} data-testid="transfer-table-row" data-table={t.target.name}>
                  <th scope="row" className="transfer-table__name">
                    {qualifiedName(t.target)}
                  </th>
                  <td>
                    <span
                      className={`badge badge--${report?.status === "done" ? "success" : report?.status === "failed" ? "danger" : report ? "warning" : "info"}`}
                      title={report?.error ?? report?.leftBehind ?? undefined}
                    >
                      {status}
                    </span>
                    {report?.error ? <p className="field__error">{report.error}</p> : null}
                    {report?.leftBehind && report.status !== "done" ? (
                      <p className="field__hint">{report.leftBehind}</p>
                    ) : null}
                  </td>
                  <td className="transfer-table__num">
                    {`${(progress?.rowsWritten ?? 0).toLocaleString("en-US")}${total !== undefined ? ` / ${total.toLocaleString("en-US")}` : ""}`}
                  </td>
                  <td className="transfer-table__num">{formatBytes(progress?.bytes ?? 0)}</td>
                  <td className="transfer-table__num">
                    {(progress?.errors ?? 0).toLocaleString("en-US")}
                  </td>
                  <td className="transfer-table__num">
                    {progress ? formatMs(progress.elapsedMs) : ""}
                  </td>
                </tr>
              );
            })}
          </tbody>
        </table>
        {run.issues.length > 0 ? (
          <div className="transfer-errors" data-testid="transfer-issues">
            <p className="transfer-label">Rows with problems</p>
            <ul className="sync-issues">
              {run.issues.map((x, i) => (
                // biome-ignore lint/suspicious/noArrayIndexKey: issues have no id.
                <li key={i}>
                  {`${p?.tables[x.table]?.target.name ?? ""} row ${x.issue.row}${x.issue.column ? ` (${x.issue.column})` : ""}: ${x.issue.message}`}
                </li>
              ))}
            </ul>
          </div>
        ) : null}
      </div>
    );
  }

  // --- navigation ---------------------------------------------------------------------------

  const stepIndex = STEPS.findIndex((s) => s.id === step);
  const canGoBack = step !== "run";
  const objectsProblem = itemsProblem(items);
  const next: Partial<Record<Step, { to: Step; ok: boolean; why?: string | undefined }>> = {
    source: {
      to: "target",
      ok:
        source.container !== null &&
        (sourceKind === "query" ? query.sql.trim() !== "" : items.length > 0),
      why: "Choose a source and at least one object",
    },
    target: { to: "objects", ok: target.container !== null, why: "Choose a target" },
    options: { to: "review", ok: modeAllowed, why: "Only new tables can be created here" },
    objects: { to: "mapping", ok: objectsProblem === null, why: objectsProblem ?? undefined },
    mapping: { to: "options", ok: currentPlan !== null },
  };

  function back(to: Step): ReactNode {
    return (
      <button type="button" className="button" onClick={() => setStep(to)}>
        <ArrowLeft {...ICON} />
        Back
      </button>
    );
  }

  let footer: ReactNode;
  if (step === "run") {
    footer = running ? (
      <button
        type="button"
        className="button button--danger"
        data-testid="transfer-cancel"
        disabled={run.cancelling || run.status === "starting"}
        onClick={cancel}
      >
        {run.cancelling ? <LoaderCircle {...ICON} className="spin" /> : <CircleX {...ICON} />}
        {run.cancelling ? "Cancelling…" : "Cancel transfer"}
      </button>
    ) : (
      <button type="button" className="button button--primary" onClick={onClose}>
        Close
      </button>
    );
  } else if (step === "review") {
    const p = currentPlan;
    footer = (
      <>
        <button type="button" className="button modal__footer-start" onClick={close}>
          Cancel
        </button>
        {back("options")}
        <button
          type="button"
          className="button button--primary"
          data-testid="transfer-start"
          disabled={
            !p || !planIsCurrent || !p.runnable || !modeAllowed || (p.destructive && !confirmed)
          }
          onClick={() => void start()}
        >
          <Play {...ICON} />
          Start transfer
        </button>
      </>
    );
  } else {
    const n = next[step];
    const previous = STEPS[stepIndex - 1]?.id;
    footer = (
      <>
        <button type="button" className="button modal__footer-start" onClick={close}>
          Cancel
        </button>
        {previous ? back(previous) : null}
        {n ? (
          <button
            type="button"
            className="button button--primary"
            data-testid="transfer-next"
            disabled={!n.ok}
            title={n.ok ? undefined : n.why}
            onClick={() => setStep(n.to)}
          >
            Next
            <ArrowRight {...ICON} />
          </button>
        ) : null}
      </>
    );
  }

  const body: Record<Step, () => ReactNode> = {
    source: sourceStep,
    target: targetStep,
    objects: objectsStep,
    mapping: mappingStep,
    options: optionsStep,
    review: reviewStep,
    run: runStep,
  };

  return (
    <Modal title="Data transfer" onClose={close} className="modal--transfer" footer={footer}>
      <ol className="transfer-steps" aria-label="Steps">
        {STEPS.map((s, i) => {
          const isDone = i < stepIndex;
          const marker = (
            <span className="transfer-steps__marker" aria-hidden="true">
              {isDone ? <Check {...ICON_SM} /> : i + 1}
            </span>
          );
          return (
            <li
              key={s.id}
              className={`transfer-steps__item${isDone ? " transfer-steps__item--done" : ""}`}
              aria-current={s.id === step ? "step" : undefined}
            >
              {isDone && canGoBack ? (
                <button
                  type="button"
                  className="transfer-steps__button"
                  title={`Back to ${s.label}`}
                  onClick={() => setStep(s.id)}
                >
                  {marker}
                  <span className="transfer-steps__label">{s.label}</span>
                </button>
              ) : (
                <span className="transfer-steps__button">
                  {marker}
                  <span className="transfer-steps__label">{s.label}</span>
                </span>
              )}
            </li>
          );
        })}
      </ol>
      {body[step]()}
    </Modal>
  );
}
