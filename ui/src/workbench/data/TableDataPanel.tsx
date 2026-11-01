/**
 * A table-data tab (M3): a table's rows opened from the object tree, filtered and sorted with
 * the builders, paged into the grid like a query result, and edited in place.
 *
 * - The rows are an ordinary run of the tab's own editor session (`table_data_open`); paging,
 *   cancel and close are the M2 run path.
 * - `edit_begin` says whether the result is editable; a read-only result shows why.
 * - Edits go into a pending-changes buffer held here (`ui/src/api/pending.ts`): changed cells,
 *   new rows and deleted rows are highlighted until Apply sends the buffer whole
 *   (`edit_apply`, one transaction) or Discard drops it. A failed apply keeps the buffer and
 *   shows each edit's error.
 * - Form view edits the current row field by field; JSON view shows the current or selected
 *   rows.
 */
import { useCallback, useEffect, useMemo, useReducer, useRef, useState } from "react";
import {
  type CellInput,
  capabilitiesGet,
  deleteRow,
  type Editability,
  type EditColumn,
  type EngineCapabilities,
  editApply,
  editBegin,
  emptyChanges,
  type Filter,
  type FormRow,
  insertRow,
  type ObjectRef,
  type PendingChanges,
  queryCancel,
  queryCloseEditor,
  removeNewRow,
  revertRow,
  rowGetForm,
  type SortKey,
  setCell,
  setNewCell,
  tableDataOpen,
} from "../../api";
import { Modal } from "../../components/Modal";
import { toCommandError } from "../../connections/api";
import { openTableDesigner } from "../layout";
import { ResultPager } from "../results/pager";
import { timedQueryPage } from "../results/perf";
import { type CellOverride, type GridEditing, ResultGrid } from "../results/ResultGrid";
import {
  type GridRowSelection,
  NO_ROWS,
  selectedRowCount,
  selectedRows,
  singleRow,
} from "../results/rowSelection";
import { IDLE_RUN, isActive, rowCountLabel, runReducer } from "../results/runState";
import { CellEditor } from "./CellEditor";
import {
  type AppliedState,
  appliedCount,
  cellEditable,
  changesSummary,
  duplicateCells,
  type EditProblem,
  editLabel,
  foldReport,
  type GridRow,
  gridRow,
  gridRowCount,
  NOTHING_APPLIED,
  pendingInput,
  problemOf,
  rowStatus,
} from "./editModel";
import { FilterBuilder } from "./FilterBuilder";
import {
  countConditions,
  draftToFilter,
  draftToSort,
  emptyDraft,
  type FilterColumn,
  type FilterDraft,
  type SortDraftKey,
} from "./filterModel";
import { type FormFieldView, JsonView, RowForm } from "./RowViews";
import { SortBuilder } from "./SortBuilder";

export interface TableDataParams {
  editorId: string;
  sessionId: string;
  table: ObjectRef;
}

type View = "grid" | "form" | "json";

/** Most rows the JSON view shows for a selection. */
export const MAX_JSON_ROWS = 100;

interface AppliedQuery {
  filter?: Filter;
  sort: SortKey[];
}

function qualified(table: ObjectRef): string {
  return [table.schema ?? table.catalog, table.name].filter(Boolean).join(".");
}

function inputOfText(text: string | null | undefined): CellInput {
  return text === null || text === undefined ? { type: "null" } : { type: "text", text };
}

/** A column for read-only results (no editability): typed as text. */
function plainColumn(name: string, index: number): EditColumn {
  return {
    name,
    index,
    dataType: { type: "text" },
    nativeType: "",
    nullable: true,
    hasDefault: false,
    key: false,
    editable: false,
    input: { type: "text" },
  };
}

function plural(n: number, one: string, many: string): string {
  return `${n.toLocaleString("en-US")} ${n === 1 ? one : many}`;
}

export function TableDataPanel({ editorId, sessionId, table }: TableDataParams) {
  const [caps, setCaps] = useState<EngineCapabilities | null>(null);
  const [run, dispatch] = useReducer(runReducer, IDLE_RUN);
  const [draft, setDraft] = useState<FilterDraft>(emptyDraft);
  const [sortDraft, setSortDraft] = useState<SortDraftKey[]>([]);
  const [query, setQuery] = useState<AppliedQuery>({ sort: [] });
  const [showBuilders, setShowBuilders] = useState(false);
  const [editability, setEditability] = useState<Editability | null>(null);
  const [editError, setEditError] = useState<string | null>(null);
  const [changes, setChanges] = useState<PendingChanges>(emptyChanges);
  const [applied, setApplied] = useState<AppliedState>(NOTHING_APPLIED);
  const [problems, setProblems] = useState<EditProblem[]>([]);
  const [applying, setApplying] = useState(false);
  const [applyError, setApplyError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [selection, setSelection] = useState<GridRowSelection>(NO_ROWS);
  const [view, setView] = useState<View>("grid");
  const [forms, setForms] = useState<ReadonlyMap<number, FormRow | { error: string }>>(new Map());
  const [confirm, setConfirm] = useState<{ text: string; action: () => void } | null>(null);
  const [filterProblems, setFilterProblems] = useState<
    ReturnType<typeof draftToFilter>["problems"]
  >([]);

  const runSeq = useRef(0);
  const runIdRef = useRef<string | null>(null);
  const pendingClose = useRef<Promise<void> | null>(null);
  const pagerRef = useRef<{ runId: string; pager: ResultPager } | null>(null);
  /** Bumped whenever the cached forms go stale (new run, apply): late answers are dropped. */
  const formsGen = useRef(0);
  const formsInFlight = useRef(new Set<string>());
  const resetForms = useCallback(() => {
    formsGen.current += 1;
    setForms(new Map());
  }, []);

  const result = run.results[0];
  const stored = result?.rowCount ?? 0;
  const resultIndex = editability?.resultIndex ?? result?.resultIndex ?? 0;
  const active = isActive(run.status);
  const editable = editability?.editable === true && !active && !applying;

  const columns = useMemo<EditColumn[]>(() => {
    const names = result?.columns ?? [];
    return names.map(
      (c, i) => editability?.columns.find((e) => e.index === i) ?? plainColumn(c.name, i),
    );
  }, [result, editability]);
  const filterColumns = useMemo<FilterColumn[]>(
    () => columns.map((c) => ({ name: c.name, input: c.input })),
    [columns],
  );

  // --- run --------------------------------------------------------------------------------

  const open = useCallback(
    async (next: AppliedQuery) => {
      runSeq.current += 1;
      const seq = runSeq.current;
      runIdRef.current = null;
      dispatch({ type: "start", script: `SELECT * FROM ${qualified(table)}` });
      setEditability(null);
      setEditError(null);
      setChanges(emptyChanges);
      setApplied(NOTHING_APPLIED);
      setProblems([]);
      setApplyError(null);
      setNotice(null);
      resetForms();
      setSelection(NO_ROWS);
      // A close sent when the tab went away (or StrictMode's remount) lands first.
      while (pendingClose.current) await pendingClose.current;
      await Promise.resolve();
      if (seq !== runSeq.current) return;
      try {
        const id = await tableDataOpen(
          sessionId,
          editorId,
          { table, ...(next.filter ? { filter: next.filter } : {}), sort: next.sort },
          (event) => {
            if (seq !== runSeq.current) return;
            dispatch({ type: "event", event });
          },
        );
        if (seq !== runSeq.current) return;
        runIdRef.current = id;
        dispatch({ type: "runId", runId: id });
      } catch (error) {
        if (seq === runSeq.current) dispatch({ type: "startFailed", error: toCommandError(error) });
      }
    },
    [sessionId, editorId, table, resetForms],
  );

  useEffect(() => {
    void open({ sort: [] });
    return () => {
      runSeq.current += 1;
      const closing = queryCloseEditor(editorId)
        .catch(() => {})
        .finally(() => {
          if (pendingClose.current === closing) pendingClose.current = null;
        });
      pendingClose.current = closing;
    };
  }, [open, editorId]);

  useEffect(() => {
    let alive = true;
    capabilitiesGet(sessionId).then(
      (c) => alive && setCaps(c),
      () => {},
    );
    return () => {
      alive = false;
    };
  }, [sessionId]);

  // Once the rows are in, ask whether they can be edited.
  const runId = run.runId;
  const finished = run.status === "finished";
  useEffect(() => {
    if (!runId || !finished) return;
    let alive = true;
    editBegin(runId).then(
      (e) => {
        if (alive) setEditability(e);
      },
      (error) => {
        if (alive) setEditError(toCommandError(error).message);
      },
    );
    return () => {
      alive = false;
    };
  }, [runId, finished]);

  // One pager per run.
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

  /** Runs `action` now, or after the user agreed to drop the pending changes. */
  function guardPending(action: () => void, what: string) {
    if (changes.edits.length === 0) {
      action();
      return;
    }
    setConfirm({
      text: `${what} discards ${plural(changes.edits.length, "pending change", "pending changes")} (${changesSummary(changes)}).`,
      action,
    });
  }

  function applyBuilders() {
    const { filter, problems: found } = draftToFilter(draft, filterColumns);
    setFilterProblems(found);
    if (found.length > 0) return;
    const next: AppliedQuery = { ...(filter ? { filter } : {}), sort: draftToSort(sortDraft) };
    guardPending(() => {
      setQuery(next);
      void open(next);
    }, "Applying the filter");
  }

  function clearBuilders() {
    setDraft(emptyDraft());
    setSortDraft([]);
    setFilterProblems([]);
    guardPending(() => {
      setQuery({ sort: [] });
      void open({ sort: [] });
    }, "Clearing the filter");
  }

  function refresh() {
    guardPending(() => void open(query), "Refreshing");
  }

  function stop() {
    const id = runIdRef.current;
    if (id) void queryCancel(id).catch(() => {});
  }

  // --- editing ----------------------------------------------------------------------------

  const commit = useCallback((row: GridRow, column: string, input: CellInput) => {
    setChanges((current) => {
      try {
        return row.type === "new"
          ? setNewCell(current, row.edit, column, input)
          : setCell(current, row.row, column, input);
      } catch {
        return current;
      }
    });
  }, []);

  const loadForm = useCallback(
    async (row: number): Promise<FormRow> => {
      if (!runId) throw new Error("no rows");
      return rowGetForm(runId, resultIndex, row);
    },
    [runId, resultIndex],
  );

  /** The value a cell shows now (pending, applied or, for stored rows, `undefined`). */
  const knownInput = useCallback(
    (row: GridRow, column: EditColumn): CellInput | undefined => {
      const pending = pendingInput(changes, row, column.name);
      if (pending) return pending;
      if (row.type === "new") return undefined;
      const values = applied.values.get(row.row);
      if (values) return inputOfText(values[column.index]);
      return undefined;
    },
    [changes, applied],
  );

  const editing = useMemo<GridEditing | undefined>(() => {
    if (!editability) return undefined;
    const rowAt = (index: number) => gridRow(index, stored, applied, changes);
    return {
      extraRows: gridRowCount(stored, applied, changes) - stored,
      override(index, col): CellOverride | undefined {
        const row = rowAt(index);
        const column = columns[col];
        if (!row || !column) return undefined;
        const input = knownInput(row, column);
        if (!input) {
          if (row.type === "new") return { text: null, placeholder: "DEFAULT" };
          return index >= stored ? { text: null } : undefined;
        }
        if (input.type === "default") return { text: null, placeholder: "DEFAULT" };
        return { text: input.type === "null" ? null : input.text };
      },
      highlight(index, col) {
        const row = rowAt(index);
        if (!row) return undefined;
        if (problemOf(problems, changes, row)) return "error";
        const status = rowStatus(changes, applied, row);
        if (status === "inserted") return "inserted";
        if (status === "deleted" || status === "removed") return "deleted";
        const column = columns[col];
        if (status === "updated" && column && pendingInput(changes, row, column.name)) {
          return "changed";
        }
        return undefined;
      },
      canEdit(index, col) {
        return editable && cellEditable(columns[col], rowAt(index), changes, applied);
      },
      renderEditor({ row: index, col, initialText, finish }) {
        const row = rowAt(index);
        const column = columns[col];
        if (!row || !column) return null;
        const known = knownInput(row, column);
        const initial: CellInput | (() => Promise<CellInput>) =
          known ??
          (row.type === "new"
            ? { type: "default" }
            : () =>
                loadForm(row.row).then((f) =>
                  inputOfText(f.fields.find((x) => x.column === column.name)?.text),
                ));
        return (
          <CellEditor
            column={column}
            initial={initial}
            initialText={initialText}
            allowDefault={row.type === "new" && column.hasDefault}
            onCommit={(input) => commit(row, column.name, input)}
            finish={finish}
          />
        );
      },
    };
  }, [
    editability,
    stored,
    applied,
    changes,
    columns,
    problems,
    editable,
    knownInput,
    loadForm,
    commit,
  ]);

  const totalRows = gridRowCount(stored, applied, changes);
  const current =
    selection.current !== null && selection.current < totalRows ? selection.current : null;
  const currentRow = current === null ? null : gridRow(current, stored, applied, changes);

  function addRow() {
    const { changes: next } = insertRow(changes);
    setChanges(next);
    setSelection(singleRow(totalRows));
  }

  async function duplicateRow() {
    if (!currentRow || !editability) return;
    let base: (column: EditColumn) => string | null | undefined;
    if (currentRow.type === "new") {
      base = (column) => {
        const input = pendingInput(changes, currentRow, column.name);
        if (!input || input.type === "default") return undefined;
        return input.type === "null" ? null : input.text;
      };
    } else {
      let form: FormRow | null = null;
      if (!applied.values.has(currentRow.row)) {
        try {
          form = await loadForm(currentRow.row);
        } catch (error) {
          setApplyError(`Could not read the row to duplicate: ${toCommandError(error).message}`);
          return;
        }
      }
      const known = (column: EditColumn) => knownInput(currentRow, column);
      base = (column) => {
        const input = known(column);
        if (input)
          return input.type === "text" ? input.text : input.type === "null" ? null : undefined;
        const field = form?.fields.find((f) => f.column === column.name);
        return field ? (field.text ?? null) : undefined;
      };
    }
    const cells = duplicateCells(editability.columns, base);
    setChanges((c) => insertRow(c, cells).changes);
    setSelection(singleRow(totalRows));
  }

  function deleteSelected() {
    // Exactly the selected rows: a selection with gaps never widens to its first..last span.
    const rows = selectedRows(selection, totalRows);
    if (rows.length === 0) return;
    let next = changes;
    // New rows go by edit index: remove from the end so earlier indexes stay valid.
    const newEdits: number[] = [];
    for (const index of rows) {
      const row = gridRow(index, stored, applied, changes);
      if (!row) continue;
      if (row.type === "new") newEdits.push(row.edit);
      else if (!applied.deleted.has(row.row)) next = deleteRow(next, row.row);
    }
    // `deleteRow` never removes edits, so the new rows' indexes still hold; removing them from
    // the last one keeps the earlier ones valid.
    for (const edit of newEdits.sort((a, b) => b - a)) next = removeNewRow(next, edit);
    setChanges(next);
  }

  function revertCurrent() {
    if (!currentRow) return;
    setChanges((c) =>
      currentRow.type === "new" ? removeNewRow(c, currentRow.edit) : revertRow(c, currentRow.row),
    );
  }

  function discard() {
    setChanges(emptyChanges);
    setProblems([]);
    setApplyError(null);
    setNotice(null);
  }

  async function apply() {
    const id = runIdRef.current;
    if (!id || changes.edits.length === 0) return;
    const sent = changes;
    setApplying(true);
    setApplyError(null);
    setNotice(null);
    try {
      const report = await editApply(id, sent);
      const folded = foldReport(sent, report, applied, stored);
      setApplied(folded.applied);
      setChanges(folded.changes);
      setProblems(folded.problems);
      resetForms();
      const warnings = report.outcomes.flatMap((o) => (o.warning ? [o.warning] : []));
      const saved = appliedCount(report);
      if (report.committed) {
        setNotice(
          `Saved ${plural(sent.edits.length, "change", "changes")}.${warnings.length ? ` ${warnings.join(" ")}` : ""}`,
        );
      } else if (saved > 0) {
        // A table that cannot roll back: the changes before the failing one were kept.
        setApplyError(
          `Saved ${saved} of ${plural(sent.edits.length, "change", "changes")}; the rest were not saved. This table's storage engine cannot roll back, so the saved changes were kept.`,
        );
      } else if (editability?.transactional === false) {
        setApplyError("Nothing was saved.");
      } else {
        setApplyError("Nothing was saved: the changes were rolled back.");
      }
    } catch (error) {
      setProblems([]);
      setApplyError(toCommandError(error).message);
    } finally {
      setApplying(false);
    }
  }

  // --- form / JSON --------------------------------------------------------------------------

  const wantedRows = useMemo(() => {
    if (view === "grid") return [];
    if (view === "form") return current === null ? [] : [current];
    return selectedRows(selection, totalRows, MAX_JSON_ROWS);
  }, [view, current, selection, totalRows]);

  useEffect(() => {
    if (!runId) return;
    const gen = formsGen.current;
    for (const row of wantedRows) {
      if (row >= stored + applied.extraRows || forms.has(row)) continue;
      const key = `${gen}:${row}`;
      if (formsInFlight.current.has(key)) continue;
      formsInFlight.current.add(key);
      const done = (entry: FormRow | { error: string }) => {
        formsInFlight.current.delete(key);
        if (formsGen.current !== gen) return;
        setForms((m) => new Map(m).set(row, entry));
      };
      rowGetForm(runId, resultIndex, row).then(done, (error) =>
        done({ error: toCommandError(error).message }),
      );
    }
  }, [runId, resultIndex, wantedRows, forms, stored, applied.extraRows]);

  function formFields(row: GridRow): { fields: FormFieldView[] | null; error: string | null } {
    let form: FormRow | null = null;
    if (row.type === "row" && !applied.values.has(row.row)) {
      const loaded = forms.get(row.row);
      if (!loaded) return { fields: null, error: null };
      if ("error" in loaded) return { fields: null, error: loaded.error };
      form = loaded;
    }
    const canEdit =
      editable &&
      rowStatus(changes, applied, row) !== "deleted" &&
      rowStatus(changes, applied, row) !== "removed";
    const fields = columns.map((column) => {
      const known = knownInput(row, column);
      const value: CellInput =
        known ??
        (row.type === "new"
          ? { type: "default" }
          : inputOfText(form?.fields.find((f) => f.column === column.name)?.text));
      return {
        column,
        value,
        pending: pendingInput(changes, row, column.name) !== undefined,
        editable: canEdit && column.editable,
        readOnlyReason: column.readOnlyReason,
      };
    });
    return { fields, error: null };
  }

  function rowJson(index: number): unknown {
    const row = gridRow(index, stored, applied, changes);
    if (!row) return undefined;
    const object: Record<string, unknown> = {};
    if (row.type === "row") {
      const loaded = forms.get(row.row);
      if (applied.values.has(row.row)) {
        const values = applied.values.get(row.row) ?? [];
        for (const column of columns) object[column.name] = values[column.index] ?? null;
      } else if (!loaded) {
        return undefined;
      } else if ("error" in loaded) {
        throw new Error(loaded.error);
      } else {
        Object.assign(object, JSON.parse(loaded.json) as Record<string, unknown>);
      }
    }
    for (const column of columns) {
      const input = pendingInput(changes, row, column.name);
      if (!input) continue;
      object[column.name] =
        input.type === "text" ? input.text : input.type === "null" ? null : "DEFAULT";
    }
    return object;
  }

  let json: string | null = null;
  let jsonError: string | null = null;
  if (view === "json" && wantedRows.length > 0) {
    try {
      const objects = wantedRows.map(rowJson);
      if (objects.every((o) => o !== undefined)) {
        json = JSON.stringify(objects.length === 1 ? objects[0] : objects, null, 2);
      }
    } catch (error) {
      jsonError = error instanceof Error ? error.message : String(error);
    }
  }

  // --- render -------------------------------------------------------------------------------

  const readOnlyReason =
    editError ??
    (editability && !editability.editable ? (editability.readOnlyReason ?? "read-only") : null);
  const pendingCount = changes.edits.length;
  const conditionCount = countConditions(draft) + (query.sort.length > 0 ? 1 : 0);
  const selectedCount = selectedRowCount(selection, totalRows);

  let status = "";
  if (run.status === "starting" || run.status === "running") status = "Loading…";
  else if (run.status === "failed") status = `Failed: ${run.error?.message ?? "unknown error"}`;
  else if (run.status === "cancelled") status = "Stopped";
  else if (result) status = rowCountLabel(result);

  const form = currentRow ? formFields(currentRow) : { fields: [], error: null };

  return (
    <section className="table-data" aria-label={`Table ${qualified(table)}`}>
      <div className="toolbar table-data__toolbar" role="toolbar" aria-label="Table data actions">
        <button type="button" className="button" onClick={refresh} disabled={active}>
          Refresh
        </button>
        <button type="button" className="button" onClick={stop} disabled={!active}>
          Stop
        </button>
        <button
          type="button"
          className="button"
          aria-expanded={showBuilders}
          onClick={() => setShowBuilders((v) => !v)}
        >
          Filter &amp; sort{conditionCount > 0 ? ` (${conditionCount})` : ""}
        </button>
        <span className="toolbar__divider" aria-hidden="true" />
        <button type="button" className="button" disabled={!editable} onClick={addRow}>
          Add row
        </button>
        <button
          type="button"
          className="button"
          disabled={!editable || !currentRow}
          onClick={() => void duplicateRow()}
        >
          Duplicate row
        </button>
        <button
          type="button"
          className="button"
          disabled={!editable || selectedCount === 0}
          onClick={deleteSelected}
        >
          Delete {selectedCount > 1 ? `${selectedCount} rows` : "row"}
        </button>
        <button
          type="button"
          className="button"
          disabled={!editable || !currentRow}
          onClick={revertCurrent}
        >
          Revert row
        </button>
        <span className="toolbar__divider" aria-hidden="true" />
        <button
          type="button"
          className="button button--primary"
          disabled={!editable || pendingCount === 0}
          onClick={() => void apply()}
          data-testid="apply-edits"
        >
          {applying ? "Applying…" : `Apply${pendingCount ? ` (${pendingCount})` : ""}`}
        </button>
        <button
          type="button"
          className="button"
          disabled={applying || (pendingCount === 0 && problems.length === 0)}
          onClick={discard}
        >
          Discard
        </button>
        <span className="toolbar__divider" aria-hidden="true" />
        <div className="segmented" role="tablist" aria-label="View">
          {(["grid", "form", "json"] as const).map((v) => (
            <button
              key={v}
              type="button"
              role="tab"
              aria-selected={view === v}
              className="segmented__item"
              onClick={() => setView(v)}
            >
              {v === "grid" ? "Grid" : v === "form" ? "Form" : "JSON"}
            </button>
          ))}
        </div>
        <button
          type="button"
          className="button"
          onClick={() => openTableDesigner({ sessionId, table })}
          disabled={table.kind !== "table"}
        >
          Design
        </button>
        <span
          className="query-editor__status"
          role="status"
          aria-live="polite"
          data-testid="table-data-status"
        >
          {status}
          {pendingCount > 0 ? ` · ${changesSummary(changes)} pending` : ""}
        </span>
      </div>

      {showBuilders ? (
        <div className="table-data__builders">
          <FilterBuilder
            draft={draft}
            columns={filterColumns}
            problems={filterProblems}
            onChange={setDraft}
            disabled={active}
          />
          <SortBuilder
            keys={sortDraft}
            columns={filterColumns}
            onChange={setSortDraft}
            nullsNative={caps?.nullsOrdering ?? true}
            disabled={active}
          />
          <div className="table-data__builder-actions">
            <button
              type="button"
              className="button button--primary"
              onClick={applyBuilders}
              disabled={active}
            >
              Apply filter
            </button>
            <button type="button" className="button" onClick={clearBuilders} disabled={active}>
              Clear
            </button>
            {filterProblems.length > 0 ? (
              <span className="filter__problem" role="alert">
                Fix the marked conditions first.
              </span>
            ) : null}
          </div>
        </div>
      ) : null}

      {readOnlyReason ? (
        <div className="banner banner--info" role="status" data-testid="read-only-banner">
          <strong>Read-only:</strong> {readOnlyReason}
        </div>
      ) : null}
      {!readOnlyReason && editability?.warning ? (
        <div className="banner banner--info" role="status" data-testid="edit-warning-banner">
          <strong>Note:</strong> {editability.warning}
        </div>
      ) : null}
      {notice ? (
        <div className="banner banner--ok" role="status">
          {notice}
        </div>
      ) : null}
      {applyError || problems.length > 0 ? (
        <div className="banner banner--error" role="alert" data-testid="apply-errors">
          {applyError ? <p className="banner__title">{applyError}</p> : null}
          {problems.length > 0 ? (
            <ul className="banner__list">
              {problems.map((p) => (
                <li key={`${p.index}`} className={`apply-problem apply-problem--${p.status}`}>
                  <strong>{editLabel(changes, p.edit)}:</strong> {p.message}
                </li>
              ))}
            </ul>
          ) : null}
        </div>
      ) : null}

      <div className="table-data__body">
        {run.status === "failed" && !result ? (
          <p className="results__empty" role="alert">
            Could not open the table: {run.error?.message}
          </p>
        ) : !result || !pager ? (
          <p className="results__empty">{active ? "Loading rows…" : "No rows."}</p>
        ) : view === "grid" ? (
          <div className="results__grid">
            <ResultGrid
              key={runId ?? ""}
              pager={pager}
              columns={result.columns}
              rowCount={stored}
              edit={editing}
              onSelectRows={setSelection}
              selectedRows={selection}
            />
          </div>
        ) : view === "form" ? (
          <RowForm
            title={
              currentRow === null
                ? "No row selected"
                : currentRow.type === "new"
                  ? `New row ${currentRow.ordinal}`
                  : `Row ${currentRow.row + 1} of ${totalRows}`
            }
            status={currentRow ? (rowStatus(changes, applied, currentRow) ?? undefined) : undefined}
            fields={form.fields}
            error={form.error}
            allowDefault={currentRow?.type === "new"}
            onChange={(column, input) => currentRow && commit(currentRow, column, input)}
            onPrev={
              current !== null && current > 0
                ? () => setSelection(singleRow(current - 1))
                : undefined
            }
            onNext={
              current !== null && current < totalRows - 1
                ? () => setSelection(singleRow(current + 1))
                : current === null && totalRows > 0
                  ? () => setSelection(singleRow(0))
                  : undefined
            }
          />
        ) : (
          <JsonView
            title={
              wantedRows.length > 1
                ? `${wantedRows.length} rows`
                : current !== null
                  ? `Row ${current + 1}`
                  : "No row selected"
            }
            json={wantedRows.length === 0 ? "" : json}
            error={jsonError}
            note={pendingCount > 0 ? "Pending values are shown as typed text." : undefined}
          />
        )}
      </div>

      {confirm ? (
        <Modal
          title="Discard pending changes?"
          onClose={() => setConfirm(null)}
          footer={
            <>
              <button type="button" className="button" onClick={() => setConfirm(null)}>
                Keep editing
              </button>
              <button
                type="button"
                className="button button--danger"
                onClick={() => {
                  const action = confirm.action;
                  setConfirm(null);
                  action();
                }}
              >
                Discard changes
              </button>
            </>
          }
        >
          <p>{confirm.text}</p>
        </Modal>
      ) : null}
    </section>
  );
}
