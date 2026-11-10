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
 * - The grid's cell menu filters by the cell's value (=, ≠, <, ≤, >, ≥, contains, one of the
 *   selected values, NULL), sets NULL, pastes (cells, or new rows) and adds, duplicates,
 *   deletes or reverts rows; its header menu sorts. Delete, Ctrl+D, Ctrl+Z, Ctrl+V and
 *   Ctrl+Shift+V do the same from the keyboard, and Ctrl+S applies the pending changes.
 * - A status line under the grid: rows loaded (and the limit), selected rows, pending changes,
 *   load time.
 * - A foreign-key cell opens the referenced row in a new tab ("Open related row"); a tab opened
 *   that way starts filtered to it (`related`).
 */
import type { Item } from "@glideapps/glide-data-grid";
import {
  ArrowDownWideNarrow,
  ArrowUpNarrowWide,
  Braces,
  Check,
  CircleAlert,
  CircleCheck,
  CircleX,
  ClipboardPaste,
  CopyPlus,
  Download,
  Eraser,
  Funnel,
  Info,
  Link,
  LoaderCircle,
  Lock,
  type LucideIcon,
  PanelRight,
  PencilRuler,
  Plus,
  RefreshCw,
  Rows3,
  Square,
  Table2,
  Trash2,
  TriangleAlert,
  Undo2,
  X,
} from "lucide-react";
import { useCallback, useEffect, useMemo, useReducer, useRef, useState } from "react";
import {
  type CellInput,
  capabilitiesGet,
  deleteRow,
  designerLoad,
  type Editability,
  type EditColumn,
  type EngineCapabilities,
  editApply,
  editBegin,
  emptyChanges,
  type Filter,
  type FilterOp,
  type ForeignKeyDef,
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
import { type MenuItem, OPENS_SUBMENU } from "../../components/ContextMenu";
import { ICON, ICON_LG, ICON_SM } from "../../components/icons";
import { Modal } from "../../components/Modal";
import { Select } from "../../components/Select";
import { toCommandError } from "../../connections/api";
import { useConnections } from "../../connections/store";
import { getSettings } from "../../settings";
import { openTableData, openTableDesigner, type RelatedValue } from "../layout";
import { qualifiedName as qualified } from "../names";
import { parseTsv, quoteIdent } from "../results/copyFormats";
import { ResultPager } from "../results/pager";
import { timedQueryPage } from "../results/perf";
import {
  type CellOverride,
  type GridCellTarget,
  type GridEditing,
  type GridKey,
  ResultGrid,
} from "../results/ResultGrid";
import {
  type GridRowSelection,
  NO_ROWS,
  selectedRowCount,
  selectedRows,
  singleRow,
} from "../results/rowSelection";
import { IDLE_RUN, isActive, rowCountLabel, runReducer } from "../results/runState";
import { registerShortcuts, type ShortcutHandler } from "../shortcuts";
import { openExport } from "../transfer/store";
import { noteUnappliedChanges } from "../unappliedChanges";
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
  emptyGroup,
  type FilterColumn,
  type FilterDraft,
  newCondition,
  newRaw,
  newSortKey,
  opsFor,
  type SortDraftKey,
} from "./filterModel";
import { type FormFieldView, JsonView, RowForm } from "./RowViews";
import {
  defaultRowLimit,
  parseRowLimit,
  ROW_LIMITS,
  rowLimitLabel,
  rowLimitValue,
} from "./rowLimit";
import { SortBuilder } from "./SortBuilder";

export interface TableDataParams {
  editorId: string;
  sessionId: string;
  table: ObjectRef;
  /** Opened from a foreign key: start filtered to the rows with these values. */
  related?: RelatedValue[] | undefined;
}

/** The filter draft of a related-rows tab: one `=` (or IS NULL) condition per column. */
function relatedDraft(related: RelatedValue[] | undefined): FilterDraft {
  const draft = emptyDraft();
  if (!related) return draft;
  for (const r of related) {
    draft.root.items.push({
      ...newCondition(r.column),
      op: r.value === null ? "isNull" : "eq",
      values: r.value === null ? [] : [r.value],
    });
  }
  return draft;
}

type View = "grid" | "form" | "json";

const VIEWS: { id: View; label: string; icon: LucideIcon }[] = [
  { id: "grid", label: "Grid", icon: Table2 },
  { id: "form", label: "Form", icon: Rows3 },
  { id: "json", label: "JSON", icon: Braces },
];

/** Most rows the JSON view shows for a selection. */
export const MAX_JSON_ROWS = 100;

interface AppliedQuery {
  filter?: Filter;
  sort: SortKey[];
  /** Rows to load (`null` = all); see ./rowLimit. */
  limit: number | null;
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

export function TableDataPanel({ editorId, sessionId, table, related }: TableDataParams) {
  const engine = useConnections().sessions.find((s) => s.id === sessionId)?.engine;
  const [valuePanel, setValuePanel] = useState(false);
  const [caps, setCaps] = useState<EngineCapabilities | null>(null);
  const [run, dispatch] = useReducer(runReducer, IDLE_RUN);
  const [draft, setDraft] = useState<FilterDraft>(() => relatedDraft(related));
  const [sortDraft, setSortDraft] = useState<SortDraftKey[]>([]);
  const [query, setQuery] = useState<AppliedQuery>(() => {
    // The columns are not known yet: the related filter names them itself.
    const { filter } = draftToFilter(relatedDraft(related), []);
    return { ...(filter ? { filter } : {}), sort: [], limit: defaultRowLimit() };
  });
  /** The query the tab opens with (the effect below runs once per editor). */
  const initialQuery = useRef(query);
  const [showBuilders, setShowBuilders] = useState(
    () => related !== undefined || getSettings().filterOpen,
  );
  /** The table's foreign keys ("Open related row"); empty until loaded, or without a designer. */
  const [foreignKeys, setForeignKeys] = useState<ForeignKeyDef[]>([]);
  const gridBox = useRef<HTMLDivElement>(null);
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
  // The result stopped at the limit: there may be more rows.
  const limitReached =
    !active && query.limit !== null && result?.done === true && stored >= query.limit;
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
          {
            table,
            ...(next.filter ? { filter: next.filter } : {}),
            sort: next.sort,
            ...(next.limit === null ? {} : { limit: next.limit }),
          },
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
    void open(initialQuery.current);
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

  // The foreign keys, for "Open related row" (tables only; engines without a designer have none).
  useEffect(() => {
    if (table.kind !== "table") return;
    let alive = true;
    designerLoad(sessionId, table).then(
      (design) => alive && setForeignKeys(design.foreignKeys.map((f) => f.def)),
      () => {},
    );
    return () => {
      alive = false;
    };
  }, [sessionId, table]);

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

  function applyBuilders(withDraft: FilterDraft = draft, withSort: SortDraftKey[] = sortDraft) {
    const { filter, problems: found } = draftToFilter(withDraft, filterColumns);
    setFilterProblems(found);
    if (found.length > 0) return;
    const next: AppliedQuery = {
      ...(filter ? { filter } : {}),
      sort: draftToSort(withSort),
      limit: query.limit,
    };
    guardPending(() => {
      setQuery(next);
      void open(next);
    }, "Applying the filter");
  }

  /** Adds `column op values` to the filter (ANDed with what is there) and applies it. */
  function quickFilter(column: string, op: FilterOp, values: string[]) {
    let base = draft;
    if (base.mode === "raw") {
      const root = emptyGroup();
      if (base.raw.trim()) root.items.push({ ...newRaw(), sql: base.raw });
      base = { ...base, mode: "builder", root };
    }
    const condition = { ...newCondition(column), op, values };
    const root =
      base.root.join === "and" && !base.root.negate
        ? { ...base.root, items: [...base.root.items, condition] }
        : {
            ...emptyGroup("and"),
            items: base.root.items.length > 0 ? [base.root, condition] : [condition],
          };
    const next: FilterDraft = { ...base, root };
    setDraft(next);
    setShowBuilders(true);
    applyBuilders(next, sortDraft);
  }

  /** Sorts by `column` alone and applies it (the header menu). */
  function sortBy(column: string, descending: boolean) {
    const next = [{ ...newSortKey(column), descending }];
    setSortDraft(next);
    applyBuilders(draft, next);
  }

  function clearBuilders() {
    setDraft(emptyDraft());
    setSortDraft([]);
    setFilterProblems([]);
    guardPending(() => {
      const next: AppliedQuery = { sort: [], limit: query.limit };
      setQuery(next);
      void open(next);
    }, "Clearing the filter");
  }

  function refresh() {
    guardPending(() => void open(query), "Refreshing");
  }

  /** This tab's limit; the default for new tabs is a setting (the Settings window). */
  function changeLimit(limit: number | null) {
    const next: AppliedQuery = { ...query, limit };
    guardPending(() => {
      setQuery(next);
      void open(next);
    }, "Changing how many rows load");
  }

  // F5 / Ctrl+R reload the rows (../shortcuts), through the same pending-changes guard.
  const shortcut = useRef<ShortcutHandler>(() => false);
  shortcut.current = (command) => {
    if (command === "new") {
      // Ctrl+N in the grid adds a row; elsewhere it opens a new query tab.
      if (!editable || !gridBox.current?.contains(document.activeElement)) return false;
      addRow();
      return true;
    }
    if (command === "save") {
      // Ctrl+S applies the pending changes, like the Apply button.
      if (editable && changes.edits.length > 0) void apply();
      return true;
    }
    if (command !== "run") return false;
    // While rows load, the key is used up, like the disabled button.
    if (!active) refresh();
    return true;
  };
  useEffect(
    () => registerShortcuts(`data-${editorId}`, (command) => shortcut.current(command)),
    [editorId],
  );

  /** Exports every row of the table with the applied filter and sort (not only the grid's). */
  function exportTable() {
    openExport({
      source: {
        type: "table",
        session: sessionId,
        query: { table, ...(query.filter ? { filter: query.filter } : {}), sort: query.sort },
      },
      ...(engine ? { engine } : {}),
      label: `${table.kind === "table" ? "Table" : "View"} ${qualified(table)}${query.filter || query.sort.length > 0 ? " (filtered and sorted as shown)" : ""}`,
      baseName: table.name,
    });
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
      valueEdit(index, col) {
        const row = rowAt(index);
        const column = columns[col];
        if (!row || !column) return undefined;
        const initial: CellInput | (() => Promise<CellInput>) =
          knownInput(row, column) ??
          (row.type === "new"
            ? { type: "default" }
            : () =>
                loadForm(row.row).then((f) =>
                  inputOfText(f.fields.find((x) => x.column === column.name)?.text),
                ));
        return {
          initial,
          nullable: column.nullable,
          onSave: (input) => commit(row, column.name, input),
        };
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

  async function duplicateRow(at: number | null = current) {
    const currentRow = at === null ? null : gridRow(at, stored, applied, changes);
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

  function deleteSelected(from: GridRowSelection = selection) {
    // Exactly the selected rows: a selection with gaps never widens to its first..last span.
    const rows = selectedRows(from, totalRows);
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

  function revertCurrent(at: number | null = current) {
    const currentRow = at === null ? null : gridRow(at, stored, applied, changes);
    if (!currentRow) return;
    setChanges((c) =>
      currentRow.type === "new" ? removeNewRow(c, currentRow.edit) : revertRow(c, currentRow.row),
    );
  }

  /** Pastes text cells from (`col`, `row`) on, row by row; "NULL" is NULL where allowed. */
  function pasteCells([col, row]: Item, values: readonly (readonly string[])[]) {
    if (!editable) return;
    let next = changes;
    let pasted = 0;
    let skipped = 0;
    values.forEach((line, r) => {
      line.forEach((text, c) => {
        const column = columns[col + c];
        const target = gridRow(row + r, stored, applied, changes);
        if (!column || !target || !cellEditable(column, target, changes, applied)) {
          skipped += 1;
          return;
        }
        const input: CellInput =
          text === "NULL" && column.nullable ? { type: "null" } : { type: "text", text };
        try {
          next =
            target.type === "new"
              ? setNewCell(next, target.edit, column.name, input)
              : setCell(next, target.row, column.name, input);
          pasted += 1;
        } catch {
          skipped += 1;
        }
      });
    });
    setChanges(next);
    setNotice(
      skipped > 0
        ? `Pasted ${plural(pasted, "cell", "cells")}; ${plural(skipped, "cell was", "cells were")} skipped (read-only, deleted or past the last row).`
        : null,
    );
  }

  /** Each pasted line becomes a new row, its cells from column `col` on. */
  function pasteRows(values: string[][], col: number) {
    if (!editable) return;
    let next = changes;
    for (const line of values) {
      const cells = line.flatMap((text, c) => {
        const column = columns[col + c];
        if (!column?.editable) return [];
        const input: CellInput =
          text === "NULL" && column.nullable ? { type: "null" } : { type: "text", text };
        return [{ column: column.name, input }];
      });
      next = insertRow(next, cells).changes;
    }
    setChanges(next);
    setSelection(singleRow(totalRows));
  }

  async function pasteFromClipboard(at: Item, asRows: boolean) {
    try {
      const values = parseTsv(await navigator.clipboard.readText());
      if (values.length === 0) return;
      if (asRows) pasteRows(values, at[0]);
      else pasteCells(at, values);
    } catch {
      setApplyError(
        `Could not read the clipboard here: press ${asRows ? "Ctrl+Shift+V" : "Ctrl+V"} in the grid.`,
      );
    }
  }

  function onGridKey(key: GridKey): boolean {
    if (!editable) return false;
    if (key === "delete") deleteSelected();
    else if (key === "duplicate") void duplicateRow();
    else revertCurrent();
    return true;
  }

  /** The table data items of the grid's cell menu (after view and copy). */
  function cellMenu(target: GridCellTarget, rows: GridRowSelection): MenuItem[] {
    const column = columns[target.col];
    if (!column) return [];
    const row = gridRow(target.row, stored, applied, changes);
    const items: MenuItem[] = [];
    if (editable && column.nullable && cellEditable(column, row, changes, applied) && row) {
      items.push({
        label: "Set NULL",
        icon: Eraser,
        onSelect: () => commit(row, column.name, { type: "null" }),
      });
    }

    const name = column.name;
    const value = target.value;
    const short = (text: string) => (text.length > 24 ? `${text.slice(0, 24)}…` : text);
    const ops = new Set(opsFor(column.input).map((o) => o.op));
    const shown = (text: string) =>
      ["integer", "decimal", "float"].includes(column.input.type)
        ? short(text)
        : `'${short(text).replace(/'/g, "''")}'`;
    const filterItems: MenuItem[] = [];
    if (typeof value === "string") {
      const add = (op: FilterOp, label: string) => {
        if (ops.has(op)) {
          filterItems.push({ label, onSelect: () => quickFilter(name, op, [value]) });
        }
      };
      add("eq", `= ${shown(value)}`);
      add("ne", `≠ ${shown(value)}`);
      add("lt", `< ${shown(value)}`);
      add("le", `≤ ${shown(value)}`);
      add("gt", `> ${shown(value)}`);
      add("ge", `≥ ${shown(value)}`);
      add("contains", `contains ${shown(value)}`);
    }
    const oneOf = (op: "in" | "notIn") => async () => {
      const values = [...new Set((await target.selectedValues()).filter((v) => v !== null))];
      if (values.length > 0) quickFilter(name, op, values as string[]);
    };
    if (ops.has("in")) {
      filterItems.push(
        {
          label: "is one of the selected values",
          separatorBefore: filterItems.length > 0,
          onSelect: () => void oneOf("in")(),
        },
        { label: "is not one of the selected values", onSelect: () => void oneOf("notIn")() },
      );
    }
    filterItems.push(
      {
        label: "is NULL",
        separatorBefore: true,
        onSelect: () => quickFilter(name, "isNull", []),
      },
      { label: "is not NULL", onSelect: () => quickFilter(name, "isNotNull", []) },
    );
    if (query.filter) {
      filterItems.push({
        label: "Clear the filter",
        separatorBefore: true,
        onSelect: clearBuilders,
      });
    }
    items.push({
      label: `Filter by ${short(name)}`,
      icon: Funnel,
      onSelect: OPENS_SUBMENU,
      submenu: filterItems,
    });

    for (const fk of foreignKeys) {
      if (!fk.columns.includes(name) || !row) continue;
      items.push({
        label: `Open related row in ${short(qualified(fk.ref_table))}`,
        icon: Link,
        separatorBefore: true,
        disabled: fk.columns.length === 1 && value === null,
        onSelect: () => void openRelated(fk, target),
      });
    }

    if (editable) {
      const at: Item = [target.col, target.row];
      const count = selectedRowCount(rows, totalRows);
      items.push(
        {
          label: "Paste",
          icon: ClipboardPaste,
          shortcut: "Ctrl+V",
          separatorBefore: true,
          onSelect: () => void pasteFromClipboard(at, false),
        },
        {
          label: "Paste as new rows",
          shortcut: "Ctrl+Shift+V",
          onSelect: () => void pasteFromClipboard(at, true),
        },
        { label: "Add row", icon: Plus, separatorBefore: true, onSelect: addRow },
        {
          label: "Duplicate row",
          icon: CopyPlus,
          shortcut: "Ctrl+D",
          onSelect: () => void duplicateRow(rows.current),
        },
        {
          label: count > 1 ? `Delete ${count} rows` : "Delete row",
          icon: Trash2,
          shortcut: "Delete",
          danger: true,
          onSelect: () => deleteSelected(rows),
        },
        {
          label: "Revert row",
          icon: Undo2,
          shortcut: "Ctrl+Z",
          onSelect: () => revertCurrent(rows.current),
        },
      );
    }
    if (items[0]) items[0] = { ...items[0], separatorBefore: true };
    return items;
  }

  /** Opens the row `fk` references from row `target.row` in a new, filtered tab. */
  async function openRelated(fk: ForeignKeyDef, target: GridCellTarget) {
    const row = gridRow(target.row, stored, applied, changes);
    if (!row) return;
    let values: (string | null)[];
    if (fk.columns.length === 1 && typeof target.value === "string") {
      values = [target.value];
    } else {
      // Several columns: their exact texts from the row (pending values first).
      let form: FormRow | null = null;
      if (row.type === "row") {
        try {
          form = await loadForm(row.row);
        } catch (error) {
          setApplyError(`Could not read the row: ${toCommandError(error).message}`);
          return;
        }
      }
      values = fk.columns.map((c) => {
        const column = columns.find((x) => x.name === c);
        const input = column ? knownInput(row, column) : undefined;
        if (input) return input.type === "text" ? input.text : null;
        return form?.fields.find((f) => f.column === c)?.text ?? null;
      });
    }
    // A NULL in a foreign key references no row.
    if (values.some((v) => v === null)) return;
    openTableData({
      sessionId,
      table: fk.ref_table,
      related: fk.ref_columns.map((column, i) => ({ column, value: values[i] ?? null })),
    });
  }

  /** The table data items of the grid's header menu: sort, filter. */
  function headerMenu(_col: number, column: { name: string }): MenuItem[] {
    return [
      {
        label: "Sort ascending",
        icon: ArrowUpNarrowWide,
        separatorBefore: true,
        onSelect: () => sortBy(column.name, false),
      },
      {
        label: "Sort descending",
        icon: ArrowDownWideNarrow,
        onSelect: () => sortBy(column.name, true),
      },
      {
        label: "Filter by this column…",
        icon: Funnel,
        onSelect: () => {
          setDraft((d) =>
            d.mode === "raw"
              ? d
              : { ...d, root: { ...d.root, items: [...d.root.items, newCondition(column.name)] } },
          );
          setShowBuilders(true);
        },
      },
    ];
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

  // Quitting the app says which tables would lose pending changes (../unappliedChanges).
  const tableName = qualified(table);
  const pendingEdits = changes.edits.length;
  useEffect(() => {
    noteUnappliedChanges(editorId, tableName, pendingEdits);
    return () => noteUnappliedChanges(editorId, tableName, 0);
  }, [editorId, tableName, pendingEdits]);

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
        <button
          type="button"
          className="button button--ghost"
          onClick={refresh}
          disabled={active}
          title="Reload the rows (F5)"
        >
          <RefreshCw {...ICON} />
          Refresh
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
          className="button button--ghost"
          aria-expanded={showBuilders}
          aria-pressed={showBuilders}
          onClick={() => setShowBuilders((v) => !v)}
          title="Filter and sort the rows"
        >
          <Funnel {...ICON} />
          Filter &amp; sort
          {conditionCount > 0 ? " " : null}
          {conditionCount > 0 ? (
            <span className="count-badge">
              <span className="visually-hidden">(</span>
              {conditionCount}
              <span className="visually-hidden">)</span>
            </span>
          ) : null}
        </button>
        <Select
          className="table-data__limit"
          aria-label="Rows to load"
          title="How many rows this tab loads (the default for new tabs is in Settings)"
          value={rowLimitValue(query.limit)}
          disabled={active}
          onChange={(e) => changeLimit(parseRowLimit(e.target.value))}
        >
          {ROW_LIMITS.map((l) => (
            <option key={rowLimitValue(l)} value={rowLimitValue(l)}>
              {rowLimitLabel(l)}
            </option>
          ))}
        </Select>
        <span className="toolbar__separator" aria-hidden="true" />
        <button
          type="button"
          className="button button--ghost"
          disabled={!editable}
          onClick={addRow}
          title="Add a new row"
        >
          <Plus {...ICON} />
          Add row
        </button>
        <button
          type="button"
          className="button button--ghost"
          disabled={!editable || !currentRow}
          onClick={() => void duplicateRow()}
          title="Add a copy of the current row"
        >
          <CopyPlus {...ICON} />
          Duplicate row
        </button>
        <button
          type="button"
          className="button button--ghost table-data__delete"
          disabled={!editable || selectedCount === 0}
          onClick={() => deleteSelected()}
          title="Mark the selected rows for deletion"
        >
          <Trash2 {...ICON} />
          Delete {selectedCount > 1 ? `${selectedCount} rows` : "row"}
        </button>
        <button
          type="button"
          className="icon-button"
          disabled={!editable || !currentRow}
          onClick={() => revertCurrent()}
          aria-label="Revert row"
          title="Revert row: drop the pending changes of the current row"
        >
          <Undo2 {...ICON} />
        </button>
        <span className="toolbar__separator" aria-hidden="true" />
        <button
          type="button"
          className="button button--primary"
          disabled={!editable || pendingCount === 0}
          onClick={() => void apply()}
          data-testid="apply-edits"
          title="Save every pending change in one transaction"
        >
          {applying ? <LoaderCircle {...ICON} className="spin" /> : <Check {...ICON} />}
          {applying ? (
            "Applying…"
          ) : (
            <>
              Apply
              {pendingCount > 0 ? " " : null}
              {pendingCount > 0 ? (
                <span className="count-badge count-badge--on-accent">
                  <span className="visually-hidden">(</span>
                  {pendingCount}
                  <span className="visually-hidden">)</span>
                </span>
              ) : null}
            </>
          )}
        </button>
        <button
          type="button"
          className="button button--ghost"
          disabled={applying || (pendingCount === 0 && problems.length === 0)}
          onClick={discard}
          title="Drop every pending change"
        >
          <X {...ICON} />
          Discard
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
        {view === "grid" ? (
          <button
            type="button"
            className="button button--ghost"
            aria-pressed={valuePanel}
            title="Show the selected cell's whole value"
            onClick={() => setValuePanel((open) => !open)}
          >
            <PanelRight {...ICON} />
            Value
          </button>
        ) : null}
        <span
          className="query-editor__status table-data__status"
          role="status"
          aria-live="polite"
          data-testid="table-data-status"
        >
          {status}
          {pendingCount > 0 ? ` · ${changesSummary(changes)} pending` : ""}
        </span>
        <button
          type="button"
          className="icon-button"
          onClick={() => openTableDesigner({ sessionId, table })}
          disabled={table.kind !== "table"}
          aria-label="Design table"
          title="Design the table"
        >
          <PencilRuler {...ICON} />
        </button>
        <button
          type="button"
          className="icon-button"
          onClick={exportTable}
          aria-label="Export"
          title="Export every row with the filter and sort applied here"
        >
          <Download {...ICON} />
        </button>
      </div>

      {showBuilders ? (
        // A form: Enter in a condition's value applies the filter (Ctrl+Enter in Raw WHERE).
        <form
          className="table-data__builders"
          aria-label="Filter and sort"
          onSubmit={(e) => {
            e.preventDefault();
            applyBuilders();
          }}
        >
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
              type="submit"
              className="button button--primary button--small"
              title="Apply the filter and sort (Enter)"
              disabled={active}
            >
              <Check {...ICON_SM} />
              Apply filter
            </button>
            <button
              type="button"
              className="button button--ghost button--small"
              onClick={clearBuilders}
              disabled={active}
            >
              Clear
            </button>
            {filterProblems.length > 0 ? (
              <span className="filter__problem" role="alert">
                <CircleAlert {...ICON_SM} />
                Fix the marked conditions first.
              </span>
            ) : null}
          </div>
        </form>
      ) : null}

      {readOnlyReason ? (
        <div className="banner banner--info" role="status" data-testid="read-only-banner">
          <Lock {...ICON_SM} className="banner__icon" />
          <p>
            <strong>Read-only:</strong> {readOnlyReason}
          </p>
        </div>
      ) : null}
      {!readOnlyReason && editability?.warning ? (
        <div className="banner banner--warning" role="status" data-testid="edit-warning-banner">
          <TriangleAlert {...ICON_SM} className="banner__icon" />
          <p>
            <strong>Note:</strong> {editability.warning}
          </p>
        </div>
      ) : null}
      {limitReached ? (
        <div className="banner banner--info" role="status" data-testid="row-limit-banner">
          <Info {...ICON_SM} className="banner__icon" />
          <p>
            Showing the first {query.limit?.toLocaleString("en-US")} rows; the table may have more.
          </p>
          <button
            type="button"
            className="button button--small banner__action"
            onClick={() => changeLimit(null)}
          >
            Load all rows
          </button>
        </div>
      ) : null}
      {notice ? (
        <div className="banner banner--ok" role="status">
          <CircleCheck {...ICON_SM} className="banner__icon" />
          <p>{notice}</p>
        </div>
      ) : null}
      {applyError || problems.length > 0 ? (
        <div className="banner banner--error" role="alert" data-testid="apply-errors">
          <CircleAlert {...ICON_SM} className="banner__icon" />
          <div className="banner__content">
            {applyError ? <p className="banner__title">{applyError}</p> : null}
            {problems.length > 0 ? (
              <ul className="banner__list apply-problems">
                {problems.map((p) => (
                  <li key={`${p.index}`} className={`apply-problem apply-problem--${p.status}`}>
                    <CircleX {...ICON_SM} className="apply-problem__icon" />
                    <span>
                      <strong>{editLabel(changes, p.edit)}:</strong> {p.message}
                    </span>
                  </li>
                ))}
              </ul>
            ) : null}
          </div>
        </div>
      ) : null}
      {pendingCount > 0 || problems.length > 0 ? (
        <ul className="change-legend" aria-label="Pending change colours">
          <li className="change-legend__item">
            <span className="change-legend__swatch change-legend__swatch--changed" />
            Changed
          </li>
          <li className="change-legend__item">
            <span className="change-legend__swatch change-legend__swatch--inserted" />
            New
          </li>
          <li className="change-legend__item">
            <span className="change-legend__swatch change-legend__swatch--deleted" />
            Deleted
          </li>
          {problems.length > 0 ? (
            <li className="change-legend__item">
              <span className="change-legend__swatch change-legend__swatch--error" />
              Failed
            </li>
          ) : null}
        </ul>
      ) : null}

      <div className="table-data__body">
        {run.status === "failed" && !result ? (
          <div className="empty-state" role="alert">
            <span className="empty-state__icon">
              <CircleAlert {...ICON_LG} />
            </span>
            <p className="empty-state__title">Could not open the table</p>
            <p className="empty-state__hint">{run.error?.message}</p>
          </div>
        ) : !result || !pager ? (
          active ? (
            <div className="table-data__loading" role="status">
              <LoaderCircle {...ICON} className="spin" />
              Loading rows…
            </div>
          ) : (
            <p className="results__empty">No rows.</p>
          )
        ) : view === "grid" ? (
          <div className="results__grid" ref={gridBox}>
            <ResultGrid
              key={runId ?? ""}
              pager={pager}
              columns={result.columns}
              rowCount={stored}
              edit={editing}
              onSelectRows={setSelection}
              selectedRows={selection}
              valuePanel={valuePanel}
              onValuePanelChange={setValuePanel}
              engine={engine ?? null}
              sqlTable={[table.schema ?? table.catalog, table.name]
                .filter((part): part is string => Boolean(part))
                .map((part) => quoteIdent(part, engine))
                .join(".")}
              cellMenu={cellMenu}
              headerMenu={headerMenu}
              onKey={onGridKey}
              onPasteCells={pasteCells}
              onPasteRows={pasteRows}
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

      {result ? (
        <footer className="table-data__statusbar" data-testid="table-data-statusbar">
          <span>
            {plural(stored, "row", "rows")}
            {limitReached ? ` (the first ${query.limit?.toLocaleString("en-US")})` : ""}
            {result.done ? "" : "…"}
          </span>
          {selectedCount > 1 ? <span>{plural(selectedCount, "row", "rows")} selected</span> : null}
          {pendingCount > 0 ? (
            <span>
              {plural(pendingCount, "pending change", "pending changes")} ({changesSummary(changes)}
              )
            </span>
          ) : null}
          {run.elapsedMs !== null ? <span>{run.elapsedMs.toLocaleString("en-US")} ms</span> : null}
          {conditionCount > 0 ? <span>Filtered</span> : null}
        </footer>
      ) : null}

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
