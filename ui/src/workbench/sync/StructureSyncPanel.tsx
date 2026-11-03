/**
 * Structure sync (docs/m8-contracts.md §5, §8.4): pick a source and a target database or schema
 * of the same engine, compare them, review the differences grouped by object kind (side by
 * side, with the DDL of both sides and the changed properties), select what to sync, preview
 * the script and apply it. Nothing changes the target before the confirmed apply; the core
 * re-compares afterwards and the view shows that result.
 */
import {
  ArrowRight,
  Check,
  CircleX,
  GitCompareArrows,
  LoaderCircle,
  Play,
  RefreshCw,
  TriangleAlert,
} from "lucide-react";
import { type ReactNode, useEffect, useId, useMemo, useRef, useState } from "react";
import {
  compareForget,
  type DiffItem,
  type ObjectKind,
  type StructureApplyReport,
  type StructureCompareResult,
  type StructureOptions,
  type SyncScript,
  structureApply,
  structureCompare,
  structureScript,
} from "../../api";
import { ICON, ICON_LG, ICON_SM } from "../../components/icons";
import { Modal } from "../../components/Modal";
import { cancelRequest, newRequestId, toCommandError } from "../../connections/api";
import { useConnections } from "../../connections/store";
import { ENGINE_INFO } from "../../connections/types";
import { languageFor } from "../editor/language";
import { SqlDiffView } from "../editor/SqlDiffView";
import { SqlEditor, type SqlEditorHandle } from "../editor/SqlEditor";
import { formatMs } from "../format";
import type { SyncPanelParams } from "../layout";
import { engineFamily } from "../nosql/engines";
import { ContainerPicker, type SideValue, sideFromInit } from "./ContainerPicker";
import {
  containerLabel,
  defaultSelection,
  destructiveSteps,
  groupItems,
  groupState,
  type ItemGroup,
  kindGroupLabel,
  orderedSelection,
  plural,
  STATUS_ACTION,
  SYNC_KIND_CHOICES,
  selectable,
  setGroup,
  toggle,
  valueText,
} from "./model";
import { Acknowledge, Note, StatusIcon, WarningList } from "./parts";

type CompareState =
  | { status: "idle" }
  | { status: "running"; requestId: string }
  | { status: "ok"; result: StructureCompareResult }
  | { status: "error"; message: string };

type ScriptState =
  | { status: "idle" }
  | { status: "loading" }
  | { status: "ok"; script: SyncScript }
  | { status: "error"; message: string };

type ApplyState =
  | { status: "idle" }
  | { status: "running"; requestId: string }
  | { status: "done"; report: StructureApplyReport }
  | { status: "error"; message: string };

interface OptionsForm {
  kinds: ObjectKind[];
  includeDrops: boolean;
  compareComments: boolean;
  compareCollations: boolean;
  compareOwnership: boolean;
  ignoreCase: boolean;
}

const DEFAULT_OPTIONS: OptionsForm = {
  kinds: [...SYNC_KIND_CHOICES],
  includeDrops: false,
  compareComments: true,
  compareCollations: true,
  compareOwnership: false,
  ignoreCase: false,
};

function checkboxState(el: HTMLInputElement | null, state: boolean | "mixed") {
  if (el) el.indeterminate = state === "mixed";
}

export function StructureSyncPanel({ params }: { params: SyncPanelParams }) {
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
  const [options, setOptions] = useState<OptionsForm>(DEFAULT_OPTIONS);
  const [compare, setCompare] = useState<CompareState>({ status: "idle" });
  const [selection, setSelection] = useState<Set<string>>(new Set());
  const [showSame, setShowSame] = useState(false);
  const [activeId, setActiveId] = useState<string | null>(null);
  const [tab, setTab] = useState<"object" | "script">("object");
  const [script, setScript] = useState<ScriptState>({ status: "idle" });
  const [confirming, setConfirming] = useState(false);
  const [acknowledged, setAcknowledged] = useState(false);
  const [apply, setApply] = useState<ApplyState>({ status: "idle" });
  const scriptEditor = useRef<SqlEditorHandle | null>(null);
  const compareIdRef = useRef<string | null>(null);
  const inflight = useRef<string | null>(null);

  const sourceSession = sqlSessions.find((s) => s.id === source.sessionId) ?? null;
  const engine = sourceSession?.engine;
  const targetSessions = sqlSessions.filter((s) => !engine || s.engine === engine);
  const result = compare.status === "ok" ? compare.result : null;
  const compareId = result?.compareId ?? null;
  const items = result?.diff.items ?? [];
  const groups = useMemo(() => groupItems(items, showSame), [items, showSame]);
  const selected = orderedSelection(items, selection);
  const selectionKey = selected.join("\n");
  const active = items.find((i) => i.id === activeId) ?? null;
  const language = languageFor(engine);
  const busy = compare.status === "running" || apply.status === "running";
  const sameContainer =
    source.sessionId === target.sessionId &&
    containerLabel(source.container) === containerLabel(target.container);
  const canCompare =
    !busy && source.container !== null && target.container !== null && !sameContainer;

  // The core keeps compares by id: forget the old one when a new one replaces it, and on close.
  useEffect(() => {
    const previous = compareIdRef.current;
    compareIdRef.current = compareId;
    if (previous && previous !== compareId) void compareForget(previous).catch(() => {});
  }, [compareId]);
  useEffect(
    () => () => {
      const last = compareIdRef.current;
      if (last) void compareForget(last).catch(() => {});
      if (inflight.current) void cancelRequest(inflight.current).catch(() => {});
    },
    [],
  );

  // The script follows the selection (it changes nothing on the target).
  useEffect(() => {
    if (!compareId || !selectionKey) {
      setScript({ status: "idle" });
      return;
    }
    let current = true;
    setScript({ status: "loading" });
    const timer = setTimeout(() => {
      structureScript({ compareId, selection: selectionKey.split("\n") }).then(
        (s) => current && setScript({ status: "ok", script: s }),
        (error) =>
          current && setScript({ status: "error", message: toCommandError(error).message }),
      );
    }, 150);
    return () => {
      current = false;
      clearTimeout(timer);
    };
  }, [compareId, selectionKey]);

  function structureOptions(): StructureOptions {
    return {
      kinds: options.kinds,
      includeDrops: options.includeDrops,
      compareComments: options.compareComments,
      compareCollations: options.compareCollations,
      compareOwnership: options.compareOwnership,
      ignoreCase: options.ignoreCase,
    };
  }

  function adopt(next: StructureCompareResult, includeDrops: boolean) {
    setCompare({ status: "ok", result: next });
    setSelection(defaultSelection(next.diff.items, includeDrops));
    const first = next.diff.items.find(selectable) ?? next.diff.items[0] ?? null;
    setActiveId((current) =>
      current && next.diff.items.some((i) => i.id === current) ? current : (first?.id ?? null),
    );
  }

  async function runCompare() {
    if (!canCompare || !source.container || !target.container) return;
    const requestId = newRequestId();
    inflight.current = requestId;
    setCompare({ status: "running", requestId });
    setApply({ status: "idle" });
    try {
      const next = await structureCompare(
        {
          sourceSession: source.sessionId,
          source: source.container,
          targetSession: target.sessionId,
          target: target.container,
          options: structureOptions(),
        },
        requestId,
      );
      if (inflight.current !== requestId) return;
      adopt(next, options.includeDrops);
    } catch (error) {
      if (inflight.current !== requestId) return;
      const err = toCommandError(error);
      setCompare(
        err.category === "cancelled"
          ? { status: "idle" }
          : { status: "error", message: err.message },
      );
    } finally {
      if (inflight.current === requestId) inflight.current = null;
    }
  }

  function cancelCompare() {
    if (compare.status === "running") void cancelRequest(compare.requestId).catch(() => {});
  }

  async function runApply() {
    if (!compareId || script.status !== "ok") return;
    const requestId = newRequestId();
    inflight.current = requestId;
    setConfirming(false);
    setApply({ status: "running", requestId });
    try {
      const report = await structureApply(
        {
          compareId,
          selection: selected,
          confirmDestructive: script.script.destructive && acknowledged,
        },
        requestId,
      );
      setApply({ status: "done", report });
      adopt(report.recompare, options.includeDrops);
    } catch (error) {
      setApply({ status: "error", message: toCommandError(error).message });
    } finally {
      if (inflight.current === requestId) inflight.current = null;
      setAcknowledged(false);
    }
  }

  // --- views ------------------------------------------------------------------------------

  function setupBar(): ReactNode {
    return (
      <div className="sync__setup">
        <ContainerPicker
          label="Source"
          sessions={sqlSessions}
          value={source}
          disabled={busy}
          testId="sync-source"
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
          testId="sync-target"
          emptyHint={
            engine
              ? `Structure sync needs two connections of the same engine (${ENGINE_INFO[engine].label}).`
              : undefined
          }
          onChange={setTarget}
        />
        <div className="sync__actions">
          {compare.status === "running" ? (
            <button type="button" className="button button--danger" onClick={cancelCompare}>
              <CircleX {...ICON} />
              Cancel
            </button>
          ) : (
            <button
              type="button"
              className="button button--primary"
              data-testid="structure-compare"
              disabled={!canCompare}
              title={sameContainer ? "Source and target are the same" : undefined}
              onClick={() => void runCompare()}
            >
              {result ? <RefreshCw {...ICON} /> : <GitCompareArrows {...ICON} />}
              {result ? "Compare again" : "Compare"}
            </button>
          )}
        </div>
      </div>
    );
  }

  function optionsRow(): ReactNode {
    const set = (patch: Partial<OptionsForm>) => setOptions({ ...options, ...patch });
    return (
      <details className="sync__options">
        <summary>Options</summary>
        <div className="sync__options-body">
          <fieldset className="fieldset sync__kinds">
            <legend>Object kinds</legend>
            {SYNC_KIND_CHOICES.map((kind) => (
              <label key={kind} className="checkbox">
                <input
                  type="checkbox"
                  checked={options.kinds.includes(kind)}
                  disabled={busy}
                  onChange={(e) =>
                    set({
                      kinds: e.target.checked
                        ? SYNC_KIND_CHOICES.filter((k) => k === kind || options.kinds.includes(k))
                        : options.kinds.filter((k) => k !== kind),
                    })
                  }
                />
                {kindGroupLabel(kind)}
              </label>
            ))}
          </fieldset>
          <fieldset className="fieldset sync__kinds">
            <legend>Compare</legend>
            <label className="checkbox">
              <input
                type="checkbox"
                checked={options.includeDrops}
                disabled={busy}
                onChange={(e) => set({ includeDrops: e.target.checked })}
              />
              Drop objects that exist only in the target
            </label>
            <label className="checkbox">
              <input
                type="checkbox"
                checked={options.compareComments}
                disabled={busy}
                onChange={(e) => set({ compareComments: e.target.checked })}
              />
              Comments
            </label>
            <label className="checkbox">
              <input
                type="checkbox"
                checked={options.compareCollations}
                disabled={busy}
                onChange={(e) => set({ compareCollations: e.target.checked })}
              />
              Collations
            </label>
            <label className="checkbox">
              <input
                type="checkbox"
                checked={options.compareOwnership}
                disabled={busy}
                onChange={(e) => set({ compareOwnership: e.target.checked })}
              />
              Ownership
            </label>
            <label className="checkbox">
              <input
                type="checkbox"
                checked={options.ignoreCase}
                disabled={busy}
                onChange={(e) => set({ ignoreCase: e.target.checked })}
              />
              Match names ignoring case
            </label>
          </fieldset>
        </div>
      </details>
    );
  }

  function summary(): ReactNode {
    if (!result) return null;
    const c = result.counts;
    return (
      <div className="sync__summary" data-testid="structure-counts">
        <span className="badge badge--success">
          {plural(c.onlyInSource, "only in source", "only in source")}
        </span>
        <span className="badge badge--warning">{`${c.different.toLocaleString("en-US")} different`}</span>
        <span className="badge badge--danger">
          {`${c.onlyInTarget.toLocaleString("en-US")} only in target`}
        </span>
        <span className="badge badge--info">{`${c.same.toLocaleString("en-US")} identical`}</span>
        <span className="sync__meta">{`compared in ${formatMs(result.elapsedMs)}`}</span>
        <span className="sync__spacer" />
        <label className="checkbox">
          <input
            type="checkbox"
            checked={showSame}
            onChange={(e) => setShowSame(e.target.checked)}
          />
          Show identical
        </label>
      </div>
    );
  }

  function itemRow(item: DiffItem): ReactNode {
    const can = selectable(item);
    const name = item.parent ? `${item.parent}.${item.name}` : item.name;
    const inSource = item.status !== "only_in_target";
    const inTarget = item.status !== "only_in_source";
    return (
      <li
        key={item.id}
        className={`sync-item${item.id === activeId ? " sync-item--active" : ""}`}
        data-testid="structure-item"
        data-item-id={item.id}
        data-status={item.status}
      >
        <input
          type="checkbox"
          aria-label={`Sync ${item.kind} ${name}`}
          checked={can && selection.has(item.id)}
          disabled={!can || busy}
          onChange={() => setSelection(toggle(selection, item.id))}
        />
        <button
          type="button"
          className="sync-item__open"
          aria-current={item.id === activeId ? "true" : undefined}
          onClick={() => {
            setActiveId(item.id);
            setTab("object");
          }}
        >
          <span className={`sync-item__name${inSource ? "" : " sync-item__name--missing"}`}>
            {inSource ? name : "—"}
          </span>
          <StatusIcon status={item.status} />
          <span className={`sync-item__name${inTarget ? "" : " sync-item__name--missing"}`}>
            {inTarget ? name : "—"}
          </span>
        </button>
      </li>
    );
  }

  function groupBlock(group: ItemGroup): ReactNode {
    const state = groupState(group, selection);
    const can = group.items.some(selectable);
    return (
      <li key={group.kind} className="sync-group">
        <div className="sync-group__head">
          <input
            type="checkbox"
            ref={(el) => checkboxState(el, state)}
            aria-label={`Sync all ${group.label.toLowerCase()}`}
            checked={state === true}
            disabled={!can || busy}
            onChange={(e) => setSelection(setGroup(selection, group, e.target.checked))}
          />
          <span className="sync-group__label">{group.label}</span>
          <span className="sync-group__count">{group.items.length}</span>
        </div>
        <ul className="sync-group__items">{group.items.map(itemRow)}</ul>
      </li>
    );
  }

  function itemList(): ReactNode {
    if (!result) return null;
    return (
      <div className="sync__list">
        <div className="sync__list-head" aria-hidden="true">
          <span>{`Source · ${containerLabel(result.diff.source)}`}</span>
          <span>{`Target · ${containerLabel(result.diff.target)}`}</span>
        </div>
        {groups.length === 0 ? (
          <p className="sync__empty" data-testid="structure-no-diffs">
            <Check {...ICON_SM} />
            No differences.
          </p>
        ) : (
          <ul className="sync__groups" data-testid="structure-items" aria-label="Differences">
            {groups.map(groupBlock)}
          </ul>
        )}
        {(result.skipped ?? []).length > 0 ? (
          <WarningList
            title="Not compared"
            items={(result.skipped ?? []).map((s) => `${s.object.name}: ${s.reason}`)}
          />
        ) : null}
      </div>
    );
  }

  function objectView(item: DiffItem | null): ReactNode {
    if (!item) return <p className="sync__empty">Select an object to see its differences.</p>;
    const props = item.properties ?? [];
    return (
      <div className="sync-object" data-testid="structure-object">
        <div className="sync-object__head">
          <StatusIcon status={item.status} label />
          <span className="sync-object__name">
            {`${item.kind.replace(/_/g, " ")} ${item.parent ? `${item.parent}.` : ""}${item.name}`}
          </span>
          <span className="sync__meta">
            {item.status === "same"
              ? "nothing to do"
              : `sync will ${STATUS_ACTION[item.status]} it`}
          </span>
        </div>
        <div className="sync-object__labels" aria-hidden="true">
          <span>Target (now)</span>
          <span>Source (after sync)</span>
        </div>
        <div className="sync-object__diff">
          <SqlDiffView
            original={item.targetDdl ?? ""}
            modified={item.sourceDdl ?? ""}
            language={language}
            ariaLabel={`DDL of ${item.name}: target and source`}
            testId="structure-ddl-diff"
          />
        </div>
        {props.length > 0 ? (
          <table className="transfer-table sync-props" data-testid="structure-properties">
            <thead>
              <tr>
                <th scope="col">Object</th>
                <th scope="col">Property</th>
                <th scope="col">Change</th>
                <th scope="col">Target</th>
                <th scope="col">Source</th>
              </tr>
            </thead>
            <tbody>
              {props.map((p, i) => (
                // biome-ignore lint/suspicious/noArrayIndexKey: properties can repeat per object.
                <tr key={i}>
                  <td className="transfer-table__mono">{p.object.name}</td>
                  <td className="transfer-table__mono">{p.prop ?? "(whole object)"}</td>
                  <td>{p.kind}</td>
                  <td className="transfer-table__mono">{valueText(p.old)}</td>
                  <td className="transfer-table__mono">{valueText(p.new)}</td>
                </tr>
              ))}
            </tbody>
          </table>
        ) : null}
      </div>
    );
  }

  function scriptView(): ReactNode {
    if (!result) return null;
    if (selected.length === 0) {
      return <p className="sync__empty">Select differences to see the sync script.</p>;
    }
    const s = script.status === "ok" ? script.script : null;
    return (
      <div className="sync-script">
        <div className="sync-script__status" role="status" data-testid="structure-script-status">
          {script.status === "loading" ? (
            <>
              <LoaderCircle {...ICON_SM} className="spin" />
              Building the script…
            </>
          ) : s ? (
            `${plural(s.script.steps.length, "statement", "statements")} for ${plural(s.items.length, "object", "objects")}${s.atomic ? " · one transaction" : ""}`
          ) : null}
        </div>
        {script.status === "error" ? (
          <Note tone="error" title="Could not build the script">
            {script.message}
          </Note>
        ) : null}
        {s && !s.transactionalDdl ? (
          <Note tone="warning" title="DDL is not transactional on this engine">
            Each statement commits on its own: if one fails, the ones before it stay applied.
          </Note>
        ) : null}
        {s?.destructive ? (
          <Note tone="error" title="Destructive steps">
            {destructiveSteps(s).join("; ")}
          </Note>
        ) : null}
        <WarningList title="Warnings" items={s?.warnings} />
        <div className="sync-script__editor">
          <SqlEditor
            initialValue=""
            value={s?.sql ?? ""}
            language={language}
            editorRef={scriptEditor}
            readOnly
            ariaLabel="Sync script"
            testId="structure-script"
          />
        </div>
      </div>
    );
  }

  function applyResult(): ReactNode {
    if (apply.status === "error") {
      return (
        <Note tone="error" title="The sync failed" testId="structure-result">
          {apply.message}
        </Note>
      );
    }
    if (apply.status !== "done") return null;
    const r = apply.report;
    const failed = r.outcome.steps.filter((s) => s.status === "failed");
    if (failed.length > 0 || !r.outcome.committed) {
      return (
        <Note tone="error" title="The sync did not complete" testId="structure-result">
          {failed
            .map((f) => `Statement ${f.index + 1}: ${f.error?.message ?? "failed"}`)
            .join("; ") || "The script was rolled back."}
          {r.outcome.atomic
            ? " Nothing was changed (rolled back)."
            : " Statements before it stay applied."}
        </Note>
      );
    }
    return r.inSync ? (
      <Note tone="ok" title="In sync" testId="structure-result" role="status">
        {`${plural(r.script.script.steps.length, "statement", "statements")} applied; a fresh compare finds no differences for the synced objects.`}
      </Note>
    ) : (
      <Note tone="warning" title="Differences remain" testId="structure-result" role="status">
        The script ran, but a fresh compare still finds differences (listed below).
      </Note>
    );
  }

  const s = script.status === "ok" ? script.script : null;
  const steps = s?.script.steps ?? [];

  return (
    <section className="sync" aria-label="Structure sync" data-testid="structure-sync">
      {setupBar()}
      {optionsRow()}
      {compare.status === "error" ? (
        <Note tone="error" title="The compare failed" testId="structure-error">
          {compare.message}
        </Note>
      ) : null}
      {compare.status === "running" ? (
        <p className="sync__running" role="status">
          <LoaderCircle {...ICON_SM} className="spin" />
          Reading both sides and comparing…
        </p>
      ) : null}
      {applyResult()}
      {summary()}
      <WarningList title="About this compare" items={result?.diff.warnings} />
      {result ? (
        <div className="sync__body">
          {itemList()}
          <div className="sync__detail">
            <div className="tabs" role="tablist" aria-label="Details">
              <button
                type="button"
                role="tab"
                id={`${id}-tab-object`}
                className="tabs__tab"
                aria-selected={tab === "object"}
                aria-controls={`${id}-panel`}
                onClick={() => setTab("object")}
              >
                Object
              </button>
              <button
                type="button"
                role="tab"
                id={`${id}-tab-script`}
                className="tabs__tab"
                aria-selected={tab === "script"}
                aria-controls={`${id}-panel`}
                data-testid="structure-script-tab"
                onClick={() => setTab("script")}
              >
                {`Sync script${selected.length > 0 ? ` (${selected.length})` : ""}`}
              </button>
            </div>
            <div
              className="sync__detail-body"
              role="tabpanel"
              id={`${id}-panel`}
              aria-labelledby={`${id}-tab-${tab}`}
            >
              {tab === "object" ? objectView(active) : scriptView()}
            </div>
          </div>
        </div>
      ) : compare.status !== "running" ? (
        <div className="empty-state">
          <span className="empty-state__icon" aria-hidden="true">
            <GitCompareArrows {...ICON_LG} />
          </span>
          <p className="empty-state__title">Compare two databases or schemas</p>
          <p className="empty-state__hint">
            Pick a source and a target of the same engine. Nothing changes until you apply the
            script.
          </p>
        </div>
      ) : null}
      {result ? (
        <div className="sync__footer">
          <span className="sync__meta" data-testid="structure-selection">
            {`${plural(selected.length, "object", "objects")} selected`}
          </span>
          <button
            type="button"
            className="button button--primary"
            data-testid="structure-apply"
            disabled={busy || selected.length === 0 || script.status !== "ok"}
            onClick={() => {
              setAcknowledged(false);
              setConfirming(true);
            }}
          >
            {apply.status === "running" ? (
              <LoaderCircle {...ICON} className="spin" />
            ) : (
              <Play {...ICON} />
            )}
            {apply.status === "running" ? "Applying…" : "Apply to target…"}
          </button>
        </div>
      ) : null}
      {confirming && s && result ? (
        <Modal
          title="Apply the sync script?"
          onClose={() => setConfirming(false)}
          className="modal--wide sync-confirm"
          footer={
            <>
              <button type="button" className="button" onClick={() => setConfirming(false)}>
                Cancel
              </button>
              <button
                type="button"
                className={`button ${s.destructive ? "button--danger" : "button--primary"}`}
                data-testid="structure-confirm"
                disabled={s.destructive && !acknowledged}
                onClick={() => void runApply()}
              >
                {s.destructive ? <TriangleAlert {...ICON} /> : <Check {...ICON} />}
                {s.destructive ? "Apply, including destructive steps" : "Apply"}
              </button>
            </>
          }
        >
          <p>
            {`${plural(steps.length, "statement", "statements")} for ${plural(selected.length, "object", "objects")} will run on `}
            <strong>{containerLabel(result.diff.target)}</strong>
            {s.atomic ? " in one transaction." : "."}
          </p>
          {!s.transactionalDdl || !s.atomic ? (
            <Note tone="warning" title="Not transactional">
              This engine commits DDL statement by statement: if one fails, the statements before it
              stay applied.
            </Note>
          ) : null}
          {s.destructive ? (
            <>
              <Note tone="error" title="These steps drop or rewrite objects or data">
                <ul className="banner__list" data-testid="structure-destructive">
                  {destructiveSteps(s).map((d, i) => (
                    // biome-ignore lint/suspicious/noArrayIndexKey: steps can repeat.
                    <li key={i}>{d}</li>
                  ))}
                </ul>
              </Note>
              <Acknowledge
                checked={acknowledged}
                onChange={setAcknowledged}
                testId="structure-acknowledge"
              >
                I understand that objects and data can be lost.
              </Acknowledge>
            </>
          ) : null}
          <pre className="designer__confirm-sql">{s.sql}</pre>
        </Modal>
      ) : null}
      <span className="visually-hidden" aria-live="polite">
        {apply.status === "running" ? "Applying the sync script" : ""}
      </span>
    </section>
  );
}
