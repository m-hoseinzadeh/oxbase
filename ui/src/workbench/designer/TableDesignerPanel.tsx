/**
 * A table designer tab (M3): columns, primary key, indexes, foreign keys, checks and options of
 * one table (or a new one), with a live DDL preview.
 *
 * - The design comes from `designer_load` (or `newTableDesign`) and is sent back whole; items
 *   keep their `origin`, so renames stay renames.
 * - Every change asks for a new preview (`designer_preview`, debounced; the previous request is
 *   cancelled). The preview shows the SQL (read-only Monaco), the validation issues, the
 *   differences and warnings for destructive and non-transactional steps.
 * - Apply asks for confirmation (and an explicit acknowledgement when the script is
 *   destructive), runs `designer_apply` and shows each step's outcome; the reloaded design
 *   replaces the edited one.
 * - What the engine cannot do (`capabilities_get`) is disabled with the reason.
 */
import {
  Check,
  CircleAlert,
  CircleCheck,
  Code,
  Columns3,
  Info,
  Link2,
  ListChecks,
  ListTree,
  LoaderCircle,
  Lock,
  type LucideIcon,
  RefreshCw,
  Settings2,
  ShieldCheck,
  Table2,
  TableProperties,
  TriangleAlert,
  X,
} from "lucide-react";
import { useCallback, useEffect, useRef, useState } from "react";
import {
  capabilitiesGet,
  type DesignApplyReport,
  type DesignPreview,
  designerApply,
  designerLoad,
  designerPreview,
  type EngineCapabilities,
  newTableDesign,
  type ObjectRef,
  type PropertyDiff,
  type TableDesign,
  type Value,
} from "../../api";
import { ICON, ICON_SM } from "../../components/icons";
import { Modal } from "../../components/Modal";
import { cancelRequest, newRequestId, toCommandError } from "../../connections/api";
import { languageFor } from "../editor/language";
import { SqlEditor, type SqlEditorHandle } from "../editor/SqlEditor";
import { openTableData, sameObject } from "../layout";
import {
  ChecksEditor,
  ColumnsEditor,
  type EditorProps,
  ForeignKeysEditor,
  IndexesEditor,
  OptionsEditor,
} from "./DesignerTabs";
import { designProblems, NO_DESIGNER, tableReadOnlyReason } from "./designModel";

export interface TableDesignerProps {
  sessionId: string;
  /** The table, or for a new table its schema / database (the name may be empty). */
  table: ObjectRef;
  isNew?: boolean | undefined;
  /** Debounce of the preview request after a change, in ms. */
  previewDelayMs?: number | undefined;
  /**
   * The designed table is now `table` (a new table was created, or the table was renamed):
   * the tab updates its params and title so it is found (and reloaded) under the new name.
   */
  onTableChanged?: ((table: ObjectRef) => void) | undefined;
}

type Tab = "columns" | "indexes" | "foreignKeys" | "checks" | "options";
const TABS: { id: Tab; label: string; icon: LucideIcon }[] = [
  { id: "columns", label: "Columns", icon: Columns3 },
  { id: "indexes", label: "Indexes", icon: ListTree },
  { id: "foreignKeys", label: "Foreign keys", icon: Link2 },
  { id: "checks", label: "Checks", icon: ShieldCheck },
  { id: "options", label: "Options", icon: Settings2 },
];

type PreviewState =
  | { status: "idle" }
  | { status: "loading"; previous: DesignPreview | null }
  | { status: "ok"; preview: DesignPreview; design: TableDesign }
  | { status: "error"; message: string };

function valueText(value: Value | undefined): string {
  if (!value) return "";
  switch (value.type) {
    case "null":
      return "NULL";
    case "json":
      return JSON.stringify(value.value);
    case "bytes":
      return `${value.value.length} bytes`;
    default:
      return String(value.value);
  }
}

function diffText(diff: PropertyDiff): string {
  const where = [diff.object.parent, diff.object.name].filter(Boolean).join(".");
  const what = diff.prop
    ? `${where} ${diff.prop}`
    : `${diff.object.kind.replace("_", " ")} ${where}`;
  if (diff.kind === "added") return `+ ${what}${diff.new ? `: ${valueText(diff.new)}` : ""}`;
  if (diff.kind === "removed") return `− ${what}${diff.old ? ` (was ${valueText(diff.old)})` : ""}`;
  return `~ ${what}: ${valueText(diff.old)} → ${valueText(diff.new)}`;
}

function plural(n: number, one: string, many: string): string {
  return `${n} ${n === 1 ? one : many}`;
}

const STEP_STATUS: Record<string, string> = {
  done: "done",
  failed: "failed",
  skipped: "skipped",
  rolledBack: "rolled back",
  notRun: "not run",
};

const STEP_BADGE: Record<string, string> = {
  done: "badge--success",
  failed: "badge--danger",
  rolledBack: "badge--warning",
};

export function TableDesignerPanel({
  sessionId,
  table,
  isNew = false,
  previewDelayMs = 300,
  onTableChanged,
}: TableDesignerProps) {
  const [caps, setCaps] = useState<EngineCapabilities | null>(null);
  const [capsError, setCapsError] = useState<string | null>(null);
  const [loaded, setLoaded] = useState<TableDesign | null>(
    isNew ? newTableDesign({ ...table, kind: "table" }) : null,
  );
  const [design, setDesign] = useState<TableDesign | null>(loaded);
  const [creating, setCreating] = useState(isNew);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [tab, setTab] = useState<Tab>("columns");
  const [previewTab, setPreviewTab] = useState<"sql" | "changes">("sql");
  const [preview, setPreview] = useState<PreviewState>({ status: "idle" });
  const [confirming, setConfirming] = useState(false);
  const [acknowledged, setAcknowledged] = useState(false);
  const [applying, setApplying] = useState(false);
  const [report, setReport] = useState<DesignApplyReport | null>(null);
  const [applyError, setApplyError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const previewRequest = useRef<string | null>(null);
  const previewSeq = useRef(0);
  const viewerRef = useRef<SqlEditorHandle | null>(null);

  useEffect(() => {
    let alive = true;
    capabilitiesGet(sessionId).then(
      (c) => alive && setCaps(c),
      (error) => alive && setCapsError(toCommandError(error).message),
    );
    return () => {
      alive = false;
    };
  }, [sessionId]);

  const load = useCallback(
    async (target: ObjectRef) => {
      setLoadError(null);
      try {
        const d = await designerLoad(sessionId, target);
        setLoaded(d);
        setDesign(d);
        setReport(null);
        setApplyError(null);
      } catch (error) {
        setLoadError(toCommandError(error).message);
      }
    },
    [sessionId],
  );

  // The design shown, for the effect below (which must not re-run when it changes).
  const loadedRef = useRef(loaded);
  loadedRef.current = loaded;

  // Load the table the tab was opened with, unless that table is already shown (the params
  // were updated to the table an apply created or renamed).
  useEffect(() => {
    if (isNew) return;
    const shown = loadedRef.current;
    if (shown && sameObject(shown.table, table)) return;
    void load(table);
  }, [isNew, table, load]);

  /** The table as it is now: the last loaded or applied design's, else the one opened. */
  const currentTable = !creating && loaded ? loaded.table : table;
  const reload = () => void load(currentTable);

  const dirty = design !== null && design !== loaded;

  // A new preview after every change (debounced); the request before it is cancelled.
  useEffect(() => {
    if (!design || !dirty) {
      // A preview still in flight is for changes that are gone (discarded or applied).
      previewSeq.current += 1;
      const old = previewRequest.current;
      previewRequest.current = null;
      if (old) void cancelRequest(old).catch(() => {});
      setPreview({ status: "idle" });
      return;
    }
    previewSeq.current += 1;
    const seq = previewSeq.current;
    setPreview((p) => ({
      status: "loading",
      previous: p.status === "ok" ? p.preview : p.status === "loading" ? p.previous : null,
    }));
    const timer = setTimeout(() => {
      const old = previewRequest.current;
      if (old) void cancelRequest(old).catch(() => {});
      const requestId = newRequestId();
      previewRequest.current = requestId;
      designerPreview(sessionId, design, requestId).then(
        (result) => {
          if (seq !== previewSeq.current) return;
          previewRequest.current = null;
          setPreview({ status: "ok", preview: result, design });
        },
        (error) => {
          if (seq !== previewSeq.current) return;
          previewRequest.current = null;
          const err = toCommandError(error);
          if (err.category !== "cancelled") setPreview({ status: "error", message: err.message });
        },
      );
    }, previewDelayMs);
    return () => clearTimeout(timer);
  }, [design, dirty, sessionId, previewDelayMs]);

  useEffect(
    () => () => {
      const id = previewRequest.current;
      if (id) void cancelRequest(id).catch(() => {});
    },
    [],
  );

  function change(next: TableDesign) {
    setDesign(next);
    setNotice(null);
  }

  function discard() {
    setDesign(loaded);
    setReport(null);
    setApplyError(null);
  }

  const current = preview.status === "ok" && preview.design === design ? preview.preview : null;
  const shown =
    preview.status === "ok"
      ? preview.preview
      : preview.status === "loading"
        ? preview.previous
        : null;
  // Problems found before asking the server (engine rules of the designer traits).
  const localIssues = design && dirty ? designProblems(design, caps?.engine) : [];
  const errors = [...localIssues, ...(current?.issues ?? [])].filter((i) => i.severity === "error");
  const steps = current?.script.steps ?? [];
  const canApply =
    current !== null && errors.length === 0 && steps.length > 0 && !applying && !!caps;
  const shownIssues = [...localIssues, ...(shown?.issues ?? [])];

  async function apply() {
    if (!design || !current) return;
    setConfirming(false);
    setApplying(true);
    setApplyError(null);
    setReport(null);
    try {
      const result = await designerApply(
        sessionId,
        design,
        current.destructive ? { confirmDestructive: true } : undefined,
      );
      setReport(result);
      if (result.design) {
        const before = creating ? null : (loaded?.table ?? table);
        setLoaded(result.design);
        setDesign(result.design);
        setCreating(false);
        if (!before || !sameObject(before, result.design.table)) {
          onTableChanged?.(result.design.table);
        }
      }
      const outcome = result.outcome;
      if (
        outcome.committed &&
        outcome.steps.every((s) => s.status === "done" || s.status === "skipped")
      ) {
        setNotice(`Applied ${plural(outcome.steps.length, "statement", "statements")}.`);
      } else if (!outcome.atomic && result.design) {
        setApplyError(
          "The change failed part way. Statements before the failure stayed applied; the design was reloaded from the database.",
        );
      } else {
        setApplyError("The change failed and was rolled back. Nothing was changed.");
      }
    } catch (error) {
      setApplyError(toCommandError(error).message);
    } finally {
      setApplying(false);
    }
  }

  const designerCaps = caps?.designer ?? NO_DESIGNER;
  const engine = caps?.engine;
  const tableLock = creating ? null : tableReadOnlyReason(loaded);
  const readOnly = caps !== null && (creating ? !designerCaps.createTable : tableLock !== null);
  const editorProps: EditorProps | null = design
    ? {
        design,
        onChange: change,
        caps: designerCaps,
        issues: [...localIssues, ...(current?.issues ?? shown?.issues ?? [])],
        engine,
        serverVersion: caps?.serverVersion,
        isNew: creating,
        disabled: !caps || applying || readOnly,
      }
    : null;

  const counts: Record<Tab, number | null> = {
    columns: design?.columns.length ?? null,
    indexes: design?.indexes.length ?? null,
    foreignKeys: design?.foreignKeys.length ?? null,
    checks: design?.checks.length ?? null,
    options: null,
  };
  const destructiveSteps = (shown?.script.steps ?? []).filter((s) => s.destructive);
  const nonTransactional = (shown?.script.steps ?? []).filter((s) => !s.transactional);
  const title = creating
    ? `New table${design?.table.name ? ` ${design.table.name}` : ""}`
    : (design?.table.name ?? table.name);

  return (
    <section className="designer" aria-label={`Design ${title}`}>
      <div className="toolbar designer__toolbar" role="toolbar" aria-label="Designer actions">
        <span className="designer__title">
          <TableProperties {...ICON} className="designer__title-icon" />
          <strong>{title}</strong>
        </span>
        <span className="toolbar__separator" aria-hidden="true" />
        <button
          type="button"
          className="button button--primary"
          disabled={!canApply}
          onClick={() => {
            setAcknowledged(false);
            setConfirming(true);
          }}
          data-testid="designer-apply"
          title={creating ? "Review and create the table" : "Review and apply the changes"}
        >
          {applying ? <LoaderCircle {...ICON} className="spin" /> : <Check {...ICON} />}
          {applying ? "Applying…" : creating ? "Create table" : "Apply"}
        </button>
        <button
          type="button"
          className="button button--ghost"
          disabled={!dirty || applying}
          onClick={discard}
          title="Drop the unsaved changes"
        >
          <X {...ICON} />
          Discard
        </button>
        {!creating ? (
          <>
            <button
              type="button"
              className="button button--ghost"
              disabled={applying}
              onClick={reload}
              title="Load the table again from the database"
            >
              <RefreshCw {...ICON} />
              Reload
            </button>
            <button
              type="button"
              className="button button--ghost"
              onClick={() => design && openTableData({ sessionId, table: design.table })}
              title="Open the table's rows"
            >
              <Table2 {...ICON} />
              Open data
            </button>
          </>
        ) : null}
        <span
          className={`query-editor__status designer__status${dirty ? " designer__status--dirty" : ""}`}
          role="status"
          aria-live="polite"
        >
          {!caps && !capsError
            ? "Loading…"
            : dirty
              ? "Unsaved changes"
              : creating
                ? ""
                : "No changes"}
        </span>
      </div>

      {capsError ? (
        <div className="banner banner--error" role="alert">
          <CircleAlert {...ICON_SM} className="banner__icon" />
          <p>Could not read what this database supports: {capsError}</p>
        </div>
      ) : null}
      {readOnly ? (
        <div className="banner banner--info" role="status" data-testid="designer-read-only">
          <Lock {...ICON_SM} className="banner__icon" />
          <p>{tableLock ?? "The designer cannot create tables on this database."}</p>
        </div>
      ) : null}
      {loadError ? (
        <div className="banner banner--error" role="alert">
          <CircleAlert {...ICON_SM} className="banner__icon" />
          <p>Could not load the table: {loadError}</p>
        </div>
      ) : null}
      {notice ? (
        <div className="banner banner--ok" role="status">
          <CircleCheck {...ICON_SM} className="banner__icon" />
          <p>{notice}</p>
        </div>
      ) : null}
      {applyError ? (
        <div className="banner banner--error" role="alert" data-testid="designer-apply-error">
          <CircleAlert {...ICON_SM} className="banner__icon" />
          <div className="banner__content">
            <p className="banner__title">{applyError}</p>
            {report ? (
              <ol className="banner__list designer__steps">
                {report.outcome.steps.map((s) => {
                  const step = shown?.script.steps[s.index];
                  return (
                    <li key={s.index} className={`designer__step designer__step--${s.status}`}>
                      <span className={`badge ${STEP_BADGE[s.status] ?? ""}`}>
                        {STEP_STATUS[s.status] ?? s.status}
                      </span>
                      <span>
                        {step?.description || s.error ? (
                          <span className="visually-hidden">{" · "}</span>
                        ) : null}
                        {step?.description ?? ""}
                        {s.error ? `${step?.description ? " · " : ""}${s.error.message}` : ""}
                      </span>
                    </li>
                  );
                })}
              </ol>
            ) : null}
            {/changed since it was loaded/i.test(applyError) ? (
              <button type="button" className="button button--small" onClick={reload}>
                <RefreshCw {...ICON_SM} />
                Reload the table
              </button>
            ) : null}
          </div>
        </div>
      ) : null}

      {!design || !editorProps ? (
        loadError ? null : (
          <div className="designer__loading" role="status">
            <LoaderCircle {...ICON} className="spin" />
            Loading the table…
          </div>
        )
      ) : (
        <div className="designer__body">
          <div className="designer__editor">
            <div className="tabs designer__tabs" role="tablist" aria-label="Designer sections">
              {TABS.map((t) => (
                <button
                  key={t.id}
                  type="button"
                  role="tab"
                  aria-selected={tab === t.id}
                  className="tabs__tab designer__tab"
                  onClick={() => setTab(t.id)}
                >
                  <t.icon {...ICON_SM} />
                  {t.label}
                  {counts[t.id] !== null ? (
                    <span className="count-badge">{counts[t.id]}</span>
                  ) : null}
                </button>
              ))}
            </div>
            <div className="designer__pane" role="tabpanel">
              {tab === "columns" ? <ColumnsEditor {...editorProps} /> : null}
              {tab === "indexes" ? <IndexesEditor {...editorProps} /> : null}
              {tab === "foreignKeys" ? <ForeignKeysEditor {...editorProps} /> : null}
              {tab === "checks" ? <ChecksEditor {...editorProps} /> : null}
              {tab === "options" ? <OptionsEditor {...editorProps} /> : null}
            </div>
          </div>

          <aside
            className="designer__preview"
            aria-label="DDL preview"
            data-testid="designer-preview"
          >
            <div className="tabs designer__tabs" role="tablist" aria-label="Preview">
              <button
                type="button"
                role="tab"
                aria-selected={previewTab === "sql"}
                className="tabs__tab designer__tab"
                onClick={() => setPreviewTab("sql")}
              >
                <Code {...ICON_SM} />
                SQL
                {shown ? <span className="count-badge">{shown.script.steps.length}</span> : null}
              </button>
              <button
                type="button"
                role="tab"
                aria-selected={previewTab === "changes"}
                className="tabs__tab designer__tab"
                onClick={() => setPreviewTab("changes")}
              >
                <ListChecks {...ICON_SM} />
                Changes
                {shown ? <span className="count-badge">{shown.diffs.length}</span> : null}
              </button>
              <span className="designer__preview-state">
                {preview.status === "loading" ? (
                  <>
                    <LoaderCircle {...ICON_SM} className="spin" />
                    Updating…
                  </>
                ) : preview.status === "idle" ? (
                  "No changes"
                ) : (
                  ""
                )}
              </span>
            </div>
            {preview.status === "error" ? (
              <div className="banner banner--error" role="alert">
                <CircleAlert {...ICON_SM} className="banner__icon" />
                <p>Preview failed: {preview.message}</p>
              </div>
            ) : null}
            {shownIssues.length > 0 ? (
              <ul className="designer__issues" aria-label="Issues">
                {shownIssues.map((i) => (
                  <li
                    key={`${i.path}:${i.message}`}
                    className={`designer__issue designer__issue--${i.severity}`}
                  >
                    {i.severity === "error" ? (
                      <CircleAlert {...ICON_SM} className="designer__issue-icon" />
                    ) : (
                      <TriangleAlert {...ICON_SM} className="designer__issue-icon" />
                    )}
                    <span>
                      <strong>{i.severity === "error" ? "Error" : "Warning"}</strong>{" "}
                      {i.path ? <code>{i.path}</code> : null} {i.message}
                    </span>
                  </li>
                ))}
              </ul>
            ) : null}
            {shown?.destructive ? (
              <div className="banner banner--warning" role="note" data-testid="destructive-warning">
                <TriangleAlert {...ICON_SM} className="banner__icon" />
                <p>
                  <strong>Destructive:</strong> {plural(destructiveSteps.length, "step", "steps")}{" "}
                  can drop or rewrite data:{" "}
                  {destructiveSteps.map((s) => s.description ?? s.sql.split("\n")[0]).join("; ")}
                </p>
              </div>
            ) : null}
            {shown && shown.script.steps.length > 0 && !shown.atomic ? (
              <div className="banner banner--warning" role="note" data-testid="non-atomic-warning">
                <TriangleAlert {...ICON_SM} className="banner__icon" />
                <p>
                  <strong>Not transactional:</strong>{" "}
                  {nonTransactional.length > 0
                    ? `${plural(nonTransactional.length, "statement commits", "statements commit")} on its own. `
                    : ""}
                  {shown.notes.length > 0
                    ? shown.notes.join(" ")
                    : "If a statement fails, the statements before it stay applied."}
                </p>
              </div>
            ) : shown?.notes.length ? (
              <div className="banner banner--info" role="note">
                <Info {...ICON_SM} className="banner__icon" />
                <p>{shown.notes.join(" ")}</p>
              </div>
            ) : null}
            {previewTab === "sql" ? (
              <div className="designer__sql">
                <SqlEditor
                  initialValue=""
                  value={shown?.sql ?? "-- No changes"}
                  language={languageFor(engine)}
                  editorRef={viewerRef}
                  readOnly
                  ariaLabel="DDL preview"
                  testId="ddl-preview"
                />
              </div>
            ) : (
              <ul className="designer__diffs" aria-label="Changes">
                {(shown?.diffs ?? []).map((d, i) => (
                  // Diffs have no identity; their order is fixed by the comparator.
                  // biome-ignore lint/suspicious/noArrayIndexKey: ordered list.
                  <li key={i} className={`designer__diff designer__diff--${d.kind}`}>
                    {diffText(d)}
                  </li>
                ))}
                {shown && shown.diffs.length === 0 ? (
                  <li className="filter__empty">No differences.</li>
                ) : null}
              </ul>
            )}
          </aside>
        </div>
      )}

      {confirming && current ? (
        <Modal
          title={creating ? "Create the table?" : "Apply the changes?"}
          onClose={() => setConfirming(false)}
          className="modal--wide designer__confirm"
          footer={
            <>
              <button type="button" className="button" onClick={() => setConfirming(false)}>
                Cancel
              </button>
              <button
                type="button"
                className={`button ${current.destructive ? "button--danger" : "button--primary"}`}
                disabled={current.destructive && !acknowledged}
                onClick={() => void apply()}
                data-testid="designer-confirm"
              >
                {current.destructive ? <TriangleAlert {...ICON} /> : <Check {...ICON} />}
                {current.destructive ? "Apply destructive changes" : "Apply"}
              </button>
            </>
          }
        >
          <p>
            {plural(steps.length, "statement", "statements")} will run on{" "}
            <strong>{design?.table.name}</strong>
            {current.atomic ? " in one transaction." : "."}
          </p>
          {!current.atomic ? (
            <div className="banner banner--warning" role="note">
              <TriangleAlert {...ICON_SM} className="banner__icon" />
              <p>Not transactional: if a statement fails, the statements before it stay applied.</p>
            </div>
          ) : null}
          {current.destructive ? (
            <>
              <div className="banner banner--error" role="note">
                <TriangleAlert {...ICON_SM} className="banner__icon" />
                <p>
                  These steps can drop or rewrite data:
                  <br />
                  {steps
                    .filter((s) => s.destructive)
                    .map((s) => s.description ?? s.sql)
                    .join("; ")}
                </p>
              </div>
              <label className="checkbox designer__ack">
                <input
                  type="checkbox"
                  checked={acknowledged}
                  onChange={(e) => setAcknowledged(e.target.checked)}
                />
                I understand that data can be lost.
              </label>
            </>
          ) : null}
          <pre className="designer__confirm-sql">{current.sql}</pre>
        </Modal>
      ) : null}
    </section>
  );
}
