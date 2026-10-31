/**
 * The result grid: Glide Data Grid (canvas, virtualized) over a {@link ResultPager}. The grid
 * asks for cells of the visible rows only; cells whose page is not loaded yet draw as loading
 * placeholders and are redrawn when the page arrives.
 *
 * With `edit` (table data, M3) the grid also shows rows past the stored ones (new rows), cell
 * values that differ from the store (pending or applied edits), highlights changed, new,
 * deleted and failed rows, and opens the caller's typed editor in the cell overlay.
 */
import {
  CompactSelection,
  DataEditor,
  type DataEditorRef,
  type DrawHeaderCallback,
  type GridCell,
  GridCellKind,
  type GridColumn,
  GridColumnIcon,
  type GridSelection,
  type Item,
  type ProvideEditorCallback,
  type ProvideEditorComponent,
  type Rectangle,
  type Theme,
} from "@glideapps/glide-data-grid";
import {
  createContext,
  type ReactNode,
  type RefObject,
  useCallback,
  useContext,
  useEffect,
  useMemo,
  useRef,
  useState,
} from "react";
import type { ColumnInfo } from "../../api";
import { type CopyNotify, cellsForSelection, NULL_COPY_TEXT } from "./copy";
import { cellDisplayText, columnTypeLabel, formatCell, initialColumnWidth } from "./format";
import type { ResultPager } from "./pager";

/** Rows assumed visible before the grid reports its real viewport. */
const MIN_VIEWPORT_ROWS = 60;

function columnIcon(column: ColumnInfo): GridColumnIcon {
  const t = column.dataType;
  if (/^(Int|UInt|Float|Decimal)/.test(t)) return GridColumnIcon.HeaderNumber;
  if (t === "Boolean") return GridColumnIcon.HeaderBoolean;
  if (/^(Timestamp|Date)/.test(t)) return GridColumnIcon.HeaderDate;
  if (/^Time/.test(t)) return GridColumnIcon.HeaderTime;
  if (/^(List|LargeList|FixedSizeList)/.test(t)) return GridColumnIcon.HeaderArray;
  if (/^(Struct|Map)/.test(t) || column.metadata?.["ARROW:extension:name"] === "arrow.json") {
    return GridColumnIcon.HeaderCode;
  }
  return GridColumnIcon.HeaderString;
}

/** `#rrggbb` with an alpha, for the selection tint (Glide parses colours itself). */
function withAlpha(hex: string, alpha: number): string {
  const m = /^#([0-9a-f]{2})([0-9a-f]{2})([0-9a-f]{2})$/i.exec(hex);
  if (!m) return `rgba(37, 99, 235, ${alpha})`;
  const [r, g, b] = [m[1], m[2], m[3]].map((h) => Number.parseInt(h ?? "0", 16));
  return `rgba(${r}, ${g}, ${b}, ${alpha})`;
}

function cssVar(style: CSSStyleDeclaration, name: string, fallback: string): string {
  const value = style.getPropertyValue(name).trim();
  return value || fallback;
}

/** Glide draws on a canvas, so it gets our CSS tokens as resolved colours. */
function readTheme(): Partial<Theme> {
  const style = getComputedStyle(document.documentElement);
  const fg = cssVar(style, "--color-fg", "#1d2129");
  const muted = cssVar(style, "--color-fg-muted", "#5c6370");
  const faint = cssVar(style, "--color-fg-faint", "#8a919c");
  const bg = cssVar(style, "--color-bg", "#ffffff");
  const subtle = cssVar(style, "--color-bg-subtle", "#f4f5f7");
  const border = cssVar(style, "--color-border", "#d8dbe1");
  const accent = cssVar(style, "--color-accent", "#2563eb");
  return {
    accentColor: accent,
    accentLight: withAlpha(accent, 0.18),
    textDark: fg,
    textMedium: muted,
    textLight: faint,
    textHeader: muted,
    textHeaderSelected: cssVar(style, "--color-accent-fg", "#ffffff"),
    bgIconHeader: muted,
    fgIconHeader: bg,
    bgCell: bg,
    bgCellMedium: subtle,
    bgHeader: subtle,
    bgHeaderHasFocus: cssVar(style, "--color-bg-muted", "#e9ebef"),
    bgHeaderHovered: cssVar(style, "--color-bg-muted", "#e9ebef"),
    borderColor: border,
    drilldownBorder: border,
    linkColor: accent,
    fontFamily: cssVar(style, "--font-mono", "monospace"),
    baseFontStyle: "12px",
    headerFontStyle: "600 12px",
    editorFontSize: "12px",
  };
}

/** Cell highlight colours of the editable grid. */
interface EditColours {
  changed: string;
  inserted: string;
  deleted: string;
  error: string;
}

function readEditColours(): EditColours {
  const style = getComputedStyle(document.documentElement);
  return {
    changed: cssVar(style, "--grid-changed-bg", "#fff4d6"),
    inserted: cssVar(style, "--grid-inserted-bg", "#e6f6ec"),
    deleted: cssVar(style, "--grid-deleted-bg", "#fdecea"),
    error: cssVar(style, "--grid-error-bg", "#f9d6d2"),
  };
}

export type CellHighlight = "changed" | "inserted" | "deleted" | "error";

/** What an edited cell shows instead of its stored value. */
export interface CellOverride {
  /** `null` = NULL. */
  text: string | null;
  /** Drawn faded (e.g. DEFAULT of a new row). */
  placeholder?: string | undefined;
}

/** The selected cell's row and the selected row range (grid rows). */
export interface GridRowSelection {
  current: number | null;
  first: number;
  last: number;
}

export interface GridEditing {
  /** Rows shown after the stored ones (new rows). */
  extraRows: number;
  /** A cell's shown value when it differs from the store (or the row is not stored). */
  override(row: number, col: number): CellOverride | undefined;
  highlight(row: number, col: number): CellHighlight | undefined;
  canEdit(row: number, col: number): boolean;
  /** The typed editor of the cell overlay; `finish(move)` closes it (moving down on Enter). */
  renderEditor(args: {
    row: number;
    col: number;
    initialText?: string | undefined;
    finish: (move: boolean) => void;
  }): ReactNode;
}

function useGridTheme(): Partial<Theme> {
  const [theme, setTheme] = useState(readTheme);
  useEffect(() => {
    const media = window.matchMedia?.("(prefers-color-scheme: dark)");
    if (!media) return;
    const update = () => setTheme(readTheme());
    media.addEventListener("change", update);
    return () => media.removeEventListener("change", update);
  }, []);
  return theme;
}

const LOADING: GridCell = { kind: GridCellKind.Loading, allowOverlay: false };

interface OverlayContext {
  edit: RefObject<GridEditing | undefined>;
  target: RefObject<Item | null>;
}

const OverlayTarget = createContext<OverlayContext | null>(null);

/** The cell overlay of the editable grid: the caller's typed editor for the target cell. */
const OverlayEditor: ProvideEditorComponent<GridCell> = ({ initialValue, onFinishedEditing }) => {
  const context = useContext(OverlayTarget);
  const [target] = useState(() => context?.target.current ?? null);
  const editing = context?.edit.current;
  if (!editing || !target) return null;
  const [col, row] = target;
  return (
    <>
      {editing.renderEditor({
        row,
        col,
        ...(initialValue === undefined ? {} : { initialText: initialValue }),
        finish: (move) => onFinishedEditing(undefined, move ? [0, 1] : [0, 0]),
      })}
    </>
  );
};

export interface ResultGridProps {
  pager: ResultPager;
  columns: ColumnInfo[];
  /** Rows stored so far; grows while the result streams. */
  rowCount: number;
  /** Tells the user about a copy that was cut short or failed. */
  onNotice?: CopyNotify | undefined;
  /** Editing (table data); absent = read-only result. */
  edit?: GridEditing | undefined;
  /** The selected row(s) changed. */
  onSelectRows?: ((selection: GridRowSelection) => void) | undefined;
}

const NO_SELECTION: GridSelection = {
  columns: CompactSelection.empty(),
  rows: CompactSelection.empty(),
};

export function ResultGrid({
  pager,
  columns,
  rowCount,
  onNotice,
  edit,
  onSelectRows,
}: ResultGridProps) {
  const gridRef = useRef<DataEditorRef>(null);
  const portalRef = useRef<HTMLDivElement>(null);
  const region = useRef<Rectangle>({ x: 0, y: 0, width: 0, height: 0 });
  const theme = useGridTheme();
  const [widths, setWidths] = useState<Record<number, number>>({});
  const [selection, setSelection] = useState<GridSelection>(NO_SELECTION);
  const colours = useMemo(readEditColours, []);
  const editRef = useRef(edit);
  editRef.current = edit;

  useEffect(() => {
    pager.setRowCount(rowCount);
  }, [pager, rowCount]);

  // Redraw the visible cells of rows that just arrived.
  useEffect(
    () =>
      pager.onLoad((first, last) => {
        const r = region.current;
        const from = Math.max(first, r.y);
        const to = Math.min(last, r.y + Math.max(r.height, MIN_VIEWPORT_ROWS) - 1);
        if (to < from) return;
        const cells: { cell: Item }[] = [];
        for (let row = from; row <= to; row += 1) {
          for (let col = 0; col < columns.length; col += 1) cells.push({ cell: [col, row] });
        }
        gridRef.current?.updateCells(cells);
      }),
    [pager, columns.length],
  );

  const gridColumns = useMemo<GridColumn[]>(
    () =>
      columns.map((column, index) => ({
        id: `${index}`,
        title: column.name,
        icon: columnIcon(column),
        width: widths[index] ?? initialColumnWidth(column),
      })),
    [columns, widths],
  );

  const storedCell = useCallback(
    (col: number, row: number): GridCell => {
      const ref = pager.cell(row, col);
      if (!ref) return LOADING;
      const value = formatCell(ref.vector, ref.index, columns[col]);
      if (value.isNull) {
        return {
          kind: GridCellKind.Text,
          data: "",
          displayData: "NULL",
          copyData: NULL_COPY_TEXT,
          allowOverlay: false,
          style: "faded",
          contentAlign: value.numeric ? "right" : "left",
        };
      }
      return {
        kind: GridCellKind.Text,
        data: value.text,
        displayData: cellDisplayText(value.text),
        allowOverlay: true,
        readonly: true,
        ...(value.numeric ? { contentAlign: "right" as const } : {}),
      };
    },
    [pager, columns],
  );

  const getCellContent = useCallback(
    ([col, row]: Item): GridCell => {
      if (!edit) return storedCell(col, row);
      const override = edit.override(row, col);
      let cell: GridCell;
      if (override) {
        const isNull = override.text === null;
        const text = override.text ?? "";
        cell = {
          kind: GridCellKind.Text,
          data: text,
          displayData: override.placeholder ?? (isNull ? "NULL" : cellDisplayText(text)),
          copyData: isNull ? NULL_COPY_TEXT : text,
          allowOverlay: true,
          ...(isNull || override.placeholder ? { style: "faded" as const } : {}),
        };
      } else {
        cell = storedCell(col, row);
        if (cell.kind === GridCellKind.Loading) return cell;
      }
      const editable = edit.canEdit(row, col);
      const highlight = edit.highlight(row, col);
      return {
        ...cell,
        allowOverlay: true,
        readonly: !editable,
        ...(highlight ? { themeOverride: { bgCell: colours[highlight] } } : {}),
      } as GridCell;
    },
    [edit, storedCell, colours],
  );

  // The overlay editor is one stable component; the cell it edits is recorded when Glide asks
  // for it (just before rendering it) and read from context.
  const editTarget = useRef<Item | null>(null);
  const overlay = useMemo<OverlayContext>(() => ({ edit: editRef, target: editTarget }), []);
  const provideEditor = useCallback<ProvideEditorCallback<GridCell>>((cell) => {
    const editing = editRef.current;
    const location = cell.location;
    if (!editing || !location) return undefined;
    if (!editing.canEdit(location[1], location[0])) return undefined;
    editTarget.current = location;
    return { editor: OverlayEditor, disablePadding: true };
  }, []);

  const selectRef = useRef(onSelectRows);
  selectRef.current = onSelectRows;
  const onGridSelectionChange = useCallback((next: GridSelection) => {
    setSelection(next);
    const notify = selectRef.current;
    if (!notify) return;
    const current = next.current;
    if (current) {
      const { y, height } = current.range;
      notify({ current: current.cell[1], first: y, last: y + Math.max(height, 1) - 1 });
      return;
    }
    const first = next.rows.first();
    const last = next.rows.last();
    if (first !== undefined && last !== undefined) notify({ current: first, first, last });
    else notify({ current: null, first: 0, last: -1 });
  }, []);

  // The column's type, right-aligned in its header when there is room for it.
  const drawHeader = useCallback<DrawHeaderCallback>(
    (args, drawContent) => {
      drawContent();
      const column = columns[args.columnIndex];
      if (!column) return;
      const { ctx, rect, theme: t } = args;
      const label = columnTypeLabel(column);
      ctx.save();
      ctx.font = `${t.headerFontStyle} ${t.fontFamily}`;
      const nameWidth = ctx.measureText(column.name).width;
      ctx.font = `11px ${t.fontFamily}`;
      const typeWidth = ctx.measureText(label).width;
      const pad = t.cellHorizontalPadding;
      const used = pad + t.headerIconSize + pad + nameWidth + 2 * pad;
      if (rect.width - used >= typeWidth + pad) {
        ctx.fillStyle = t.textLight;
        ctx.textBaseline = "middle";
        ctx.textAlign = "right";
        ctx.fillText(label, rect.x + rect.width - pad, rect.y + rect.height / 2);
      }
      ctx.restore();
    },
    [columns],
  );

  // Copying reads the selection from the core (see ./copy), never "#LOADING" placeholders.
  const noticeRef = useRef(onNotice);
  noticeRef.current = onNotice;
  const getCellsForSelection = useCallback(
    (selection: Rectangle, signal: AbortSignal) =>
      cellsForSelection(pager, columns, rowCount, selection, signal, (message) =>
        noticeRef.current?.(message),
      ),
    [pager, columns, rowCount],
  );

  const onVisibleRegionChanged = useCallback(
    (range: Rectangle) => {
      region.current = range;
      pager.setVisible(range.y, range.y + Math.max(range.height, 1) - 1);
    },
    [pager],
  );

  // Before the grid reports a viewport, load the first screen.
  useEffect(() => {
    if (region.current.height === 0) pager.setVisible(0, MIN_VIEWPORT_ROWS - 1);
  }, [pager]);

  const columnSummary = columns.map((c) => `${c.name} (${columnTypeLabel(c)})`).join(", ");

  return (
    <div className="result-grid" data-testid="result-grid">
      <OverlayTarget.Provider value={overlay}>
        <DataEditor
          ref={gridRef}
          columns={gridColumns}
          rows={rowCount + (edit?.extraRows ?? 0)}
          getCellContent={getCellContent}
          drawHeader={drawHeader}
          onVisibleRegionChanged={onVisibleRegionChanged}
          onColumnResize={(_column, newSize, index) =>
            setWidths((current) => ({ ...current, [index]: newSize }))
          }
          getCellsForSelection={getCellsForSelection}
          gridSelection={selection}
          onGridSelectionChange={onGridSelectionChange}
          {...(edit
            ? {
                provideEditor,
                onDelete: () => false,
                onPaste: false,
              }
            : {})}
          rowMarkers="number"
          smoothScrollX
          smoothScrollY
          theme={theme}
          width="100%"
          height="100%"
          portalElementRef={portalRef as RefObject<HTMLElement>}
        />
      </OverlayTarget.Provider>
      <div ref={portalRef} className="result-grid__portal" />
      <span className="visually-hidden">Columns: {columnSummary}</span>
    </div>
  );
}
