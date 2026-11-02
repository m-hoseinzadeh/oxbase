/**
 * A collection tab (MongoDB, docs/m7-contracts.md §3.1, §4.2, §5.1): the documents of a
 * collection or view, queried with the find / aggregate builder, shown in the grid (Arrow pages
 * of an ordinary run, `$document` hidden), as a tree or as Extended JSON, and edited one
 * document at a time.
 *
 * - Runs are `document_find` / `document_aggregate` on the tab's own editor session (events,
 *   pages, cancel and close as `query_run`).
 * - The tree and JSON views and the editors read the current row's whole document with
 *   `document_at` (canonical Extended JSON text, kept as text so field order survives).
 * - Edits are optimistic (`document_apply`): a document changed by someone else since it was
 *   read is a conflict and nothing is written. Edits need the find mode without a projection
 *   (an aggregate or projected row is not the stored document).
 */
import {
  ArrowDown,
  ArrowUp,
  Braces,
  CircleAlert,
  CircleCheck,
  FilePlus2,
  ListTree,
  LoaderCircle,
  type LucideIcon,
  Pencil,
  Play,
  Plus,
  RefreshCw,
  Square,
  Table2,
  Trash2,
  TriangleAlert,
  X,
} from "lucide-react";
import { useCallback, useEffect, useMemo, useReducer, useRef, useState } from "react";
import {
  type AggregateRequest,
  type DocumentApplyReport,
  type DocumentEdit,
  documentAggregate,
  documentApply,
  documentAt,
  documentFind,
  type FindRequest,
  type ObjectRef,
  queryCancel,
  queryCloseEditor,
  type RunEvent,
} from "../../api";
import { ICON, ICON_LG, ICON_SM } from "../../components/icons";
import { Modal } from "../../components/Modal";
import { toCommandError } from "../../connections/api";
import { SqlEditor, type SqlEditorHandle } from "../editor/SqlEditor";
import { qualifiedName } from "../names";
import { ResultPager } from "../results/pager";
import { timedQueryPage } from "../results/perf";
import { ResultGrid } from "../results/ResultGrid";
import { type GridRowSelection, NO_ROWS } from "../results/rowSelection";
import { IDLE_RUN, isActive, type ResultInfo, runReducer } from "../results/runState";
import {
  aggregateRequest,
  type BuilderProblem,
  EMPTY_FIND,
  type FindDraft,
  findRequest,
  pipelineWrites,
  requestPreview,
  STAGE_OPERATORS,
  STAGE_TEMPLATES,
  type StageDraft,
} from "./builder";
import { DocumentJson, type DocumentState, DocumentTree } from "./DocumentViews";
import { documentId, parseOrdered, prettyJson } from "./ejson";
import { JsonField } from "./JsonField";

export interface CollectionPanelProps {
  editorId: string;
  sessionId: string;
  collection: ObjectRef;
}

type Mode = "find" | "aggregate";
type View = "grid" | "tree" | "json";

type Query =
  | { type: "find"; request: FindRequest }
  | { type: "aggregate"; request: AggregateRequest };

const VIEWS: { id: View; label: string; icon: LucideIcon }[] = [
  { id: "grid", label: "Grid", icon: Table2 },
  { id: "tree", label: "Tree", icon: ListTree },
  { id: "json", label: "JSON", icon: Braces },
];

/** What the edit dialog works on. */
type DialogState =
  | { type: "edit"; row: number; id: string; previous: string }
  | { type: "insert"; initial: string };

type Confirm = { title: string; text: string; action: string; run: () => void };

/** One line per failed or conflicting edit, in the user's words. */
export function reportProblem(report: DocumentApplyReport): string | null {
  for (const outcome of report.outcomes) {
    switch (outcome.status) {
      case "applied":
      case "notRun":
        continue;
      case "conflict":
        return "Someone changed or deleted this document after it was read, so nothing was written. Refresh to see the current version, then edit it again.";
      case "failed":
        return `The server refused the change: ${outcome.error ?? "unknown error"}`;
    }
  }
  return null;
}

let stageIds = 0;

function newStage(operator = "$match"): StageDraft {
  stageIds += 1;
  return { id: stageIds, operator, body: STAGE_TEMPLATES[operator] ?? "{}", enabled: true };
}

/** "12 documents", with the streaming / truncated state. */
export function documentCountLabel(result: ResultInfo): string {
  const n = result.rowCount;
  const count = `${n.toLocaleString("en-US")} ${n === 1 ? "document" : "documents"}`;
  if (!result.done) return `${count}…`;
  return result.truncated ? `${count} (truncated)` : count;
}

export function CollectionPanel({ editorId, sessionId, collection }: CollectionPanelProps) {
  const [mode, setMode] = useState<Mode>("find");
  const [find, setFind] = useState<FindDraft>(EMPTY_FIND);
  const [stages, setStages] = useState<StageDraft[]>(() => [newStage()]);
  const [problems, setProblems] = useState<BuilderProblem[]>([]);
  const [showBuilder, setShowBuilder] = useState(true);
  const [run, dispatch] = useReducer(runReducer, IDLE_RUN);
  const [query, setQuery] = useState<Query>({ type: "find", request: { collection } });
  const [selection, setSelection] = useState<GridRowSelection>(NO_ROWS);
  const [view, setView] = useState<View>("grid");
  const [doc, setDoc] = useState<DocumentState>({ status: "none" });
  const [dialog, setDialog] = useState<DialogState | null>(null);
  const [confirm, setConfirm] = useState<Confirm | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const runSeq = useRef(0);
  const runIdRef = useRef<string | null>(null);
  const pendingClose = useRef<Promise<void> | null>(null);
  const pagerRef = useRef<{ runId: string; pager: ResultPager } | null>(null);

  const result = run.results[0];
  const active = isActive(run.status);
  const isCollection = collection.kind === "table";
  const editable =
    isCollection && query.type === "find" && !query.request.projection && !active && !busy;
  const current =
    selection.current !== null && result && selection.current < result.rowCount
      ? selection.current
      : null;

  // --- run --------------------------------------------------------------------------------

  const start = useCallback(
    async (next: Query, confirmDestructive = false) => {
      runSeq.current += 1;
      const seq = runSeq.current;
      runIdRef.current = null;
      setQuery(next);
      setSelection(NO_ROWS);
      setDoc({ status: "none" });
      dispatch({ type: "start", script: requestPreview(collection, next) });
      while (pendingClose.current) await pendingClose.current;
      await Promise.resolve();
      if (seq !== runSeq.current) return;
      try {
        const handler = (event: RunEvent) => {
          if (seq === runSeq.current) dispatch({ type: "event", event });
        };
        const options = confirmDestructive ? { confirmDestructive: true } : undefined;
        const id =
          next.type === "find"
            ? await documentFind(sessionId, editorId, next.request, handler, options)
            : await documentAggregate(sessionId, editorId, next.request, handler, options);
        if (seq !== runSeq.current) return;
        runIdRef.current = id;
        dispatch({ type: "runId", runId: id });
      } catch (err) {
        if (seq === runSeq.current) dispatch({ type: "startFailed", error: toCommandError(err) });
      }
    },
    [sessionId, editorId, collection],
  );

  useEffect(() => {
    void start({ type: "find", request: { collection } });
    return () => {
      runSeq.current += 1;
      const closing = queryCloseEditor(editorId)
        .catch(() => {})
        .finally(() => {
          if (pendingClose.current === closing) pendingClose.current = null;
        });
      pendingClose.current = closing;
    };
  }, [start, editorId, collection]);

  const runId = run.runId;
  let pager: ResultPager | null = null;
  if (runId && result) {
    if (pagerRef.current?.runId !== runId) {
      pagerRef.current?.pager.dispose();
      const index = result.resultIndex;
      pagerRef.current = {
        runId,
        pager: new ResultPager((offset, limit) => timedQueryPage(runId, index, offset, limit)),
      };
    }
    pager = pagerRef.current.pager;
  }
  useEffect(() => () => pagerRef.current?.pager.dispose(), []);

  function runBuilder() {
    setNotice(null);
    setError(null);
    if (mode === "find") {
      const built = findRequest(collection, find);
      setProblems(built.problems);
      if (built.request) void start({ type: "find", request: built.request });
      return;
    }
    const built = aggregateRequest(collection, stages);
    setProblems(built.problems);
    const request = built.request;
    if (!request) return;
    if (pipelineWrites(stages)) {
      setConfirm({
        title: "Run a pipeline that writes?",
        text: "The pipeline has a $out or $merge stage: it writes its output to a collection and can replace that collection's documents.",
        action: "Run and write",
        run: () => void start({ type: "aggregate", request }, true),
      });
      return;
    }
    void start({ type: "aggregate", request });
  }

  function stop() {
    const id = runIdRef.current;
    if (id) void queryCancel(id).catch(() => {});
  }

  // --- current document ---------------------------------------------------------------------

  const resultIndex = result?.resultIndex ?? 0;
  useEffect(() => {
    if (current === null || !runId || view === "grid") {
      if (current === null) setDoc({ status: "none" });
      return;
    }
    let alive = true;
    setDoc({ status: "loading" });
    documentAt(runId, resultIndex, current).then(
      (text) => alive && setDoc({ status: "ok", row: current, text }),
      (err) => alive && setDoc({ status: "error", message: toCommandError(err).message }),
    );
    return () => {
      alive = false;
    };
  }, [runId, resultIndex, current, view]);

  async function readCurrent(): Promise<{ row: number; text: string } | null> {
    if (current === null || !runId) return null;
    if (doc.status === "ok" && doc.row === current) return { row: current, text: doc.text };
    try {
      const text = await documentAt(runId, resultIndex, current);
      return { row: current, text };
    } catch (err) {
      setError(`Could not read the document: ${toCommandError(err).message}`);
      return null;
    }
  }

  async function editCurrent() {
    const found = await readCurrent();
    if (!found) return;
    const id = documentId(found.text);
    if (id === null) {
      setError("This document has no _id, so it cannot be edited here.");
      return;
    }
    setDialog({ type: "edit", row: found.row, id, previous: found.text });
  }

  async function apply(edits: DocumentEdit[], done: string): Promise<boolean> {
    setBusy(true);
    setError(null);
    setNotice(null);
    try {
      const report = await documentApply(sessionId, collection, edits);
      const problem = reportProblem(report);
      if (problem) {
        setError(problem);
        return false;
      }
      setNotice(done);
      void start(query);
      return true;
    } catch (err) {
      setError(toCommandError(err).message);
      return false;
    } finally {
      setBusy(false);
    }
  }

  async function deleteCurrent() {
    const found = await readCurrent();
    if (!found) return;
    const id = documentId(found.text);
    if (id === null) {
      setError("This document has no _id, so it cannot be deleted here.");
      return;
    }
    setConfirm({
      title: "Delete this document?",
      text: `Document ${found.row + 1} (_id ${id}) is deleted from ${qualifiedName(collection)}, unless it changed since it was read. This cannot be undone.`,
      action: "Delete",
      run: () =>
        void apply([{ type: "delete", id, previous: found.text }], "Deleted the document."),
    });
  }

  // --- render -------------------------------------------------------------------------------

  const problemOf = (field: string) => problems.find((p) => p.field === field)?.message;
  const rowCount = result?.rowCount ?? 0;
  let status = "";
  if (active) status = "Loading…";
  else if (run.status === "failed") status = `Failed: ${run.error?.message ?? "unknown error"}`;
  else if (run.status === "cancelled") status = "Stopped";
  else if (result) status = documentCountLabel(result);
  const editHint = !isCollection
    ? "Views are read-only."
    : query.type !== "find"
      ? "Aggregate results are not stored documents: switch to Find to edit."
      : query.request.projection
        ? "A projected document is not the whole document: clear the projection to edit."
        : undefined;

  return (
    <section
      className="collection"
      aria-label={`Collection ${qualifiedName(collection)}`}
      data-testid="collection-tab"
    >
      <div className="toolbar table-data__toolbar" role="toolbar" aria-label="Collection actions">
        <div className="segmented" role="tablist" aria-label="Query type">
          {(["find", "aggregate"] as const).map((m) => (
            <button
              key={m}
              type="button"
              role="tab"
              aria-selected={mode === m}
              className="segmented__item"
              onClick={() => {
                setMode(m);
                setShowBuilder(true);
              }}
            >
              {m === "find" ? "Find" : "Aggregate"}
            </button>
          ))}
        </div>
        <button
          type="button"
          className="button button--primary"
          data-testid="collection-run"
          disabled={active}
          onClick={runBuilder}
          title="Run the query (Ctrl+Enter in a field)"
        >
          <Play {...ICON_SM} fill="currentColor" />
          Run
        </button>
        <button
          type="button"
          className="icon-button"
          onClick={stop}
          disabled={!active}
          aria-label="Stop"
          title="Stop loading"
        >
          <Square {...ICON} />
        </button>
        <button
          type="button"
          className="icon-button"
          onClick={() => void start(query)}
          disabled={active}
          aria-label="Refresh"
          title="Run the last query again"
        >
          <RefreshCw {...ICON} />
        </button>
        <span className="toolbar__separator" aria-hidden="true" />
        <button
          type="button"
          className="button button--ghost"
          disabled={!isCollection || active || busy}
          onClick={() => setDialog({ type: "insert", initial: "{\n  \n}" })}
          title={isCollection ? "Insert a new document" : "Views are read-only"}
        >
          <FilePlus2 {...ICON} />
          Insert
        </button>
        <button
          type="button"
          className="button button--ghost"
          data-testid="document-edit"
          disabled={!editable || current === null}
          onClick={() => void editCurrent()}
          title={editHint ?? "Edit the selected document"}
        >
          <Pencil {...ICON} />
          Edit
        </button>
        <button
          type="button"
          className="button button--ghost table-data__delete"
          disabled={!editable || current === null}
          onClick={() => void deleteCurrent()}
          title={editHint ?? "Delete the selected document"}
        >
          <Trash2 {...ICON} />
          Delete
        </button>
        <span className="toolbar__separator" aria-hidden="true" />
        <div className="segmented" role="tablist" aria-label="View">
          {VIEWS.map((v) => (
            <button
              key={v.id}
              type="button"
              role="tab"
              aria-selected={view === v.id}
              className="segmented__item"
              onClick={() => setView(v.id)}
            >
              <v.icon {...ICON_SM} />
              {v.label}
            </button>
          ))}
        </div>
        <button
          type="button"
          className="button button--ghost"
          aria-expanded={showBuilder}
          onClick={() => setShowBuilder((v) => !v)}
        >
          {showBuilder ? "Hide builder" : "Show builder"}
        </button>
        <span
          className="query-editor__status table-data__status"
          role="status"
          aria-live="polite"
          data-testid="collection-status"
        >
          {status}
        </span>
      </div>

      {showBuilder ? (
        <div className="collection__builder" data-testid="query-builder">
          {mode === "find" ? (
            <FindBuilder draft={find} onChange={setFind} problemOf={problemOf} onRun={runBuilder} />
          ) : (
            <PipelineBuilder
              stages={stages}
              onChange={setStages}
              problemOf={problemOf}
              onRun={runBuilder}
            />
          )}
        </div>
      ) : null}

      {notice ? (
        <div className="banner banner--ok" role="status">
          <CircleCheck {...ICON_SM} className="banner__icon" />
          <p>{notice}</p>
        </div>
      ) : null}
      {error ? (
        <div className="banner banner--error" role="alert" data-testid="document-error">
          <CircleAlert {...ICON_SM} className="banner__icon" />
          <p>{error}</p>
        </div>
      ) : null}

      <div className="table-data__body">
        {run.status === "failed" && !result ? (
          <div className="empty-state" role="alert">
            <span className="empty-state__icon">
              <CircleAlert {...ICON_LG} />
            </span>
            <p className="empty-state__title">The query failed</p>
            <p className="empty-state__hint">{run.error?.message}</p>
          </div>
        ) : !result || !pager ? (
          active ? (
            <div className="table-data__loading" role="status">
              <LoaderCircle {...ICON} className="spin" />
              Loading documents…
            </div>
          ) : (
            <p className="results__empty">No documents.</p>
          )
        ) : (
          <div className={`collection__body collection__body--${view}`}>
            <div className="results__grid">
              <ResultGrid
                key={runId ?? ""}
                pager={pager}
                columns={result.columns}
                rowCount={rowCount}
                onSelectRows={setSelection}
                selectedRows={selection}
              />
            </div>
            {view === "grid" ? null : (
              <div className="collection__document">
                {view === "tree" ? <DocumentTree state={doc} /> : <DocumentJson state={doc} />}
              </div>
            )}
          </div>
        )}
      </div>

      {dialog ? (
        <DocumentDialog
          state={dialog}
          collection={collection}
          onCancel={() => setDialog(null)}
          onSave={async (text) => {
            const edit: DocumentEdit =
              dialog.type === "edit"
                ? { type: "replace", id: dialog.id, previous: dialog.previous, document: text }
                : { type: "insert", document: text };
            const ok = await apply(
              [edit],
              dialog.type === "edit" ? "Saved the document." : "Inserted the document.",
            );
            if (ok) setDialog(null);
            return ok;
          }}
        />
      ) : null}
      {confirm ? (
        <Modal
          title={confirm.title}
          onClose={() => setConfirm(null)}
          footer={
            <>
              <button type="button" className="button" onClick={() => setConfirm(null)}>
                Cancel
              </button>
              <button
                type="button"
                className="button button--danger"
                data-autofocus
                onClick={() => {
                  const action = confirm.run;
                  setConfirm(null);
                  action();
                }}
              >
                {confirm.action}
              </button>
            </>
          }
        >
          <p className="confirm__text">
            <TriangleAlert {...ICON_SM} className="confirm__icon" />
            {confirm.text}
          </p>
        </Modal>
      ) : null}
    </section>
  );
}

const FIND_FIELDS: { field: "filter" | "projection" | "sort"; label: string; hint: string }[] = [
  { field: "filter", label: "Filter", hint: '{"status": "active", "n": {"$gt": 1}}' },
  { field: "projection", label: "Projection", hint: '{"name": 1, "_id": 0}' },
  { field: "sort", label: "Sort", hint: '{"created": -1} (key order is the sort order)' },
];

function FindBuilder({
  draft,
  onChange,
  problemOf,
  onRun,
}: {
  draft: FindDraft;
  onChange: (draft: FindDraft) => void;
  problemOf: (field: string) => string | undefined;
  onRun: () => void;
}) {
  const latest = useRef(draft);
  latest.current = draft;
  return (
    <div className="find-builder">
      {FIND_FIELDS.map((f) => (
        <JsonField
          key={f.field}
          label={f.label}
          placeholder={f.hint}
          value={draft[f.field]}
          onChange={(text) => onChange({ ...latest.current, [f.field]: text })}
          problem={problemOf(f.field)}
          lines={f.field === "filter" ? 3 : 2}
          testId={`find-${f.field}`}
          onRun={onRun}
        />
      ))}
      <div className="find-builder__counts">
        {(["skip", "limit"] as const).map((field) => (
          <label key={field} className="find-builder__count">
            <span>{field === "skip" ? "Skip" : "Limit"}</span>
            <input
              type="text"
              inputMode="numeric"
              className="input--port"
              aria-invalid={Boolean(problemOf(field))}
              placeholder={field === "limit" ? "all" : "0"}
              value={draft[field]}
              onChange={(e) => onChange({ ...latest.current, [field]: e.target.value })}
              onKeyDown={(e) => {
                if (e.key === "Enter") onRun();
              }}
            />
            {problemOf(field) ? (
              <span className="json-field__problem" role="alert">
                {problemOf(field)}
              </span>
            ) : null}
          </label>
        ))}
      </div>
    </div>
  );
}

function PipelineBuilder({
  stages,
  onChange,
  problemOf,
  onRun,
}: {
  stages: StageDraft[];
  onChange: (stages: StageDraft[]) => void;
  problemOf: (field: string) => string | undefined;
  onRun: () => void;
}) {
  const latest = useRef(stages);
  latest.current = stages;
  const update = (id: number, patch: Partial<StageDraft>) =>
    onChange(latest.current.map((s) => (s.id === id ? { ...s, ...patch } : s)));
  const move = (index: number, by: number) => {
    const next = [...latest.current];
    const [stage] = next.splice(index, 1);
    if (!stage) return;
    next.splice(index + by, 0, stage);
    onChange(next);
  };
  return (
    <div className="pipeline" data-testid="pipeline">
      <ol className="pipeline__stages" aria-label="Pipeline stages">
        {stages.map((stage, index) => (
          <li
            key={stage.id}
            className={`pipeline__stage${stage.enabled ? "" : " pipeline__stage--off"}`}
          >
            <div className="pipeline__stage-head">
              <span className="pipeline__number">{index + 1}</span>
              <input
                type="text"
                list="pipeline-operators"
                aria-label={`Stage ${index + 1} operator`}
                aria-invalid={Boolean(problemOf(`stage:${stage.id}:operator`))}
                className="pipeline__operator"
                value={stage.operator}
                onChange={(e) => {
                  const operator = e.target.value;
                  const template = STAGE_TEMPLATES[operator];
                  const unchanged =
                    stage.body.trim() === "" || stage.body === STAGE_TEMPLATES[stage.operator];
                  update(stage.id, {
                    operator,
                    ...(template !== undefined && unchanged ? { body: template } : {}),
                  });
                }}
              />
              <label className="checkbox pipeline__enabled">
                <input
                  type="checkbox"
                  checked={stage.enabled}
                  onChange={(e) => update(stage.id, { enabled: e.target.checked })}
                />
                On
              </label>
              <span className="pipeline__actions">
                <button
                  type="button"
                  className="icon-button"
                  aria-label={`Move stage ${index + 1} up`}
                  title="Move up"
                  disabled={index === 0}
                  onClick={() => move(index, -1)}
                >
                  <ArrowUp {...ICON_SM} />
                </button>
                <button
                  type="button"
                  className="icon-button"
                  aria-label={`Move stage ${index + 1} down`}
                  title="Move down"
                  disabled={index === stages.length - 1}
                  onClick={() => move(index, 1)}
                >
                  <ArrowDown {...ICON_SM} />
                </button>
                <button
                  type="button"
                  className="icon-button"
                  aria-label={`Remove stage ${index + 1}`}
                  title="Remove the stage"
                  onClick={() => onChange(latest.current.filter((s) => s.id !== stage.id))}
                >
                  <X {...ICON_SM} />
                </button>
              </span>
            </div>
            {problemOf(`stage:${stage.id}:operator`) ? (
              <p className="json-field__problem" role="alert">
                {problemOf(`stage:${stage.id}:operator`)}
              </p>
            ) : null}
            <JsonField
              key={`${stage.id}:${stage.operator}`}
              label={`Stage ${index + 1} ${stage.operator}`}
              value={stage.body}
              onChange={(text) => update(stage.id, { body: text })}
              problem={problemOf(`stage:${stage.id}`)}
              lines={3}
              testId={`stage-${index + 1}`}
              onRun={onRun}
            />
          </li>
        ))}
      </ol>
      <datalist id="pipeline-operators">
        {STAGE_OPERATORS.map((op) => (
          <option key={op} value={op} />
        ))}
      </datalist>
      <button
        type="button"
        className="button button--ghost button--small"
        onClick={() => onChange([...latest.current, newStage()])}
      >
        <Plus {...ICON_SM} />
        Add stage
      </button>
      {stages.length === 0 ? (
        <p className="field__hint">An empty pipeline returns every document.</p>
      ) : null}
    </div>
  );
}

/** Edits or inserts one document as Extended JSON text. */
function DocumentDialog({
  state,
  collection,
  onCancel,
  onSave,
}: {
  state: DialogState;
  collection: ObjectRef;
  onCancel: () => void;
  onSave: (text: string) => Promise<boolean>;
}) {
  const editorRef = useRef<SqlEditorHandle | null>(null);
  const [problem, setProblem] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);
  const initial = useMemo(
    () => (state.type === "edit" ? prettyJson(state.previous) : state.initial),
    [state],
  );

  async function save() {
    const text = editorRef.current?.getText() ?? initial;
    try {
      const node = parseOrdered(text);
      if (node.kind !== "object") {
        setProblem("A document is an object: { … }.");
        return;
      }
    } catch (error) {
      setProblem((error as Error).message);
      return;
    }
    setProblem(null);
    setSaving(true);
    try {
      await onSave(text);
    } finally {
      setSaving(false);
    }
  }

  return (
    <Modal
      title={
        state.type === "edit"
          ? `Edit document ${state.row + 1} of ${qualifiedName(collection)}`
          : `Insert into ${qualifiedName(collection)}`
      }
      onClose={onCancel}
      className="modal--wide"
      footer={
        <>
          <button type="button" className="button" onClick={onCancel}>
            Cancel
          </button>
          <button
            type="button"
            className="button button--primary"
            data-testid="document-save"
            disabled={saving}
            onClick={() => void save()}
          >
            {saving ? <LoaderCircle {...ICON_SM} className="spin" /> : null}
            {state.type === "edit" ? "Save" : "Insert"}
          </button>
        </>
      }
    >
      <p className="field__hint">
        Extended JSON: {'{"$oid": "…"}'} for ObjectId, {'{"$date": "2026-01-31T00:00:00Z"}'} for
        dates, {'{"$numberLong": "…"}'} for 64-bit integers.
        {state.type === "edit"
          ? " The save fails without writing if someone changed the document since it was read."
          : ""}
      </p>
      <div className="document-dialog__editor">
        <SqlEditor
          initialValue={initial}
          language="javascript"
          editorRef={editorRef}
          ariaLabel="Document"
          testId="document-editor"
          onCommand={() => void save()}
        />
      </div>
      {problem ? (
        <p className="field__error" role="alert">
          {problem}
        </p>
      ) : null}
    </Modal>
  );
}
