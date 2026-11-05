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
import type { ColumnInfo, Engine } from "../../api";
import { normalizeColor, subscribeTheme } from "../theme-bridge";
import { type CopyNotify, cellsForSelection, NULL_COPY_TEXT } from "./copy";
import {
  cellDisplayText,
  columnTypeLabel,
  formatCell,
  initialColumnWidth,
  visibleColumns,
} from "./format";
import { dataCellOf } from "./gridCoords";
import type { ResultPager } from "./pager";
import { fromGridSelection, type GridRowSelection, toGridSelection } from "./rowSelection";
import { ValuePanel } from "./ValuePanel";

/**
 * The grid's own keys, minus Ctrl+Enter ("scroll to the selected cell"): Ctrl+Enter runs the
 * query wherever the focus is in the tab (../shortcuts), the grid included.
 */
const GRID_KEYS = { scrollToSelectedCell: false } as const;

/** Rows assumed visible before the grid reports its real viewport. */
const MIN_VIEWPORT_ROWS = 60;
/** Dense grid metrics (docs/design-system.md: 28 px controls, 4 px grid). */
const ROW_HEIGHT = 28;
const HEADER_HEIGHT = 32;
/** NULL draws as a faint italic word, so it never reads as the text "NULL". */
const NULL_FONT_STYLE = "italic 12.5px";

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
function withAlpha(colour: string, alpha: number): string {
  // Built CSS is minified (`#fff`), so normalise before reading the channels.
  const hex = normalizeColor(colour) ?? "";
  const m = /^#([0-9a-f]{2})([0-9a-f]{2})([0-9a-f]{2})/i.exec(hex);
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
  // Selection is "where you are": the focus blue, not the green action accent.
  const accent = cssVar(style, "--color-focus", "#38bdf8");
  return {
    accentColor: accent,
    accentLight: withAlpha(accent, 0.18),
    textDark: fg,
    textMedium: muted,
    textLight: faint,
    textHeader: muted,
    textHeaderSelected: fg,
    textBubble: fg,
    textGroupHeader: muted,
    bgIconHeader: muted,
    fgIconHeader: bg,
    bgCell: bg,
    bgCellMedium: subtle,
    bgHeader: subtle,
    bgHeaderHasFocus: cssVar(style, "--color-bg-muted", "#e9ebef"),
    bgHeaderHovered: cssVar(style, "--color-bg-muted", "#e9ebef"),
    bgBubble: cssVar(style, "--color-bg-muted", "#e9ebef"),
    bgBubbleSelected: bg,
    bgSearchResult: withAlpha(cssVar(style, "--color-warning", "#fbbf24"), 0.24),
    borderColor: border,
    horizontalBorderColor: border,
    drilldownBorder: border,
    linkColor: accent,
    fontFamily: cssVar(style, "--font-mono", "monospace"),
    baseFontStyle: "12.5px",
    headerFontStyle: "600 12px",
    editorFontSize: "12.5px",
    cellHorizontalPadding: 10,
    cellVerticalPadding: 4,
    headerIconSize: 16,
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

export type { GridRowSelection } from "./rowSelection";

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

function useEditColours(): EditColours {
  const [colours, setColours] = useState(readEditColours);
  useEffect(() => subscribeTheme(() => setColours(readEditColours())), []);
  return colours;
}

function useGridTheme(): Partial<Theme> {
  const [theme, setTheme] = useState(readTheme);
  // Theme changes (user choice or OS while "system") re-read the CSS tokens.
  useEffect(() => subscribeTheme(() => setTheme(readTheme())), []);
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
  /** The result's columns; a trailing whole-document column (`$document`) is not shown. */
  columns: ColumnInfo[];
  /** Rows stored so far; grows while the result streams. */
  rowCount: number;
  /** Tells the user about a copy that was cut short or failed. */
  onNotice?: CopyNotify | undefined;
  /** Editing (table data); absent = read-only result. */
  edit?: GridEditing | undefined;
  /** The selected row(s) changed (the exact rows, gaps included). */
  onSelectRows?: ((selection: GridRowSelection) => void) | undefined;
  /**
   * The row selection the caller holds. When it changes to something the grid did not report
   * itself (a new row, Prev / Next in the form view, a remount), the grid shows it and scrolls
   * the current row into view.
   */
  selectedRows?: GridRowSelection | undefined;
  /**
   * The value panel (the selected cell's whole value, ./ValuePanel) is open. Leave it unset and
   * the grid keeps the state itself; either way activating a read-only cell (double-click,
   * Enter, Space) opens it, and its close button closes it.
   */
  valuePanel?: boolean | undefined;
  onValuePanelChange?: ((open: boolean) => void) | undefined;
  /** The connection's engine, for the value panel's SQL beautifier. */
  engine?: Engine | null | undefined;
}

const NO_SELECTION: GridSelection = {
  columns: CompactSelection.empty(),
  rows: CompactSelection.empty(),
};

export function ResultGrid({
  pager,
  columns: resultColumns,
  rowCount,
  onNotice,
  edit,
  onSelectRows,
  selectedRows,
  valuePanel,
  onValuePanelChange,
  engine,
}: ResultGridProps) {
  // The document column is always last, so the shown columns keep their page indexes.
  const columns = useMemo(() => visibleColumns(resultColumns), [resultColumns]);
  const gridRef = useRef<DataEditorRef>(null);
  const portalRef = useRef<HTMLDivElement>(null);
  const region = useRef<Rectangle>({ x: 0, y: 0, width: 0, height: 0 });
  const theme = useGridTheme();
  const [widths, setWidths] = useState<Record<number, number>>({});
  const [selection, setSelection] = useState<GridSelection>(() =>
    selectedRows ? toGridSelection(selectedRows) : NO_SELECTION,
  );
  const colours = useEditColours();
  const editRef = useRef(edit);
  editRef.current = edit;
  const [ownValuePanel, setOwnValuePanel] = useState(false);
  const valueOpen = valuePanel ?? ownValuePanel;
  const setValueOpen = useCallback(
    (open: boolean) => {
      if (valuePanel === undefined) setOwnValuePanel(open);
      onValuePanelChange?.(open);
    },
    [valuePanel, onValuePanelChange],
  );
  // The value panel re-reads its cell when rows arrive (a page loaded after the selection).
  const [, setLoads] = useState(0);

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
        setLoads((n) => n + 1);
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

  // NULL: the faint text colour in italics (not the faded style, which would drop below 4.5:1).
  const nullTheme = useMemo<Partial<Theme>>(
    () => ({
      ...(theme.textLight ? { textDark: theme.textLight } : {}),
      baseFontStyle: NULL_FONT_STYLE,
    }),
    [theme.textLight],
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
          themeOverride: nullTheme,
          contentAlign: value.numeric ? "right" : "left",
        };
      }
      return {
        kind: GridCellKind.Text,
        data: value.text,
        displayData: cellDisplayText(value.text),
        // Read-only cells open the value panel instead of Glide's small text overlay; the
        // editable grid turns the overlay (its editor) back on below.
        allowOverlay: false,
        readonly: true,
        ...(value.numeric ? { contentAlign: "right" as const } : {}),
      };
    },
    [pager, columns, nullTheme],
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
          ...(override.placeholder
            ? { style: "faded" as const }
            : isNull
              ? { themeOverride: nullTheme }
              : {}),
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
        ...(highlight
          ? { themeOverride: { ...cell.themeOverride, bgCell: colours[highlight] } }
          : {}),
      } as GridCell;
    },
    [edit, storedCell, colours, nullTheme],
  );

  // The overlay editor is one stable component; the cell it edits is recorded when Glide asks
  // for it (just before rendering it) and read from context.
  const editTarget = useRef<Item | null>(null);
  const overlay = useMemo<OverlayContext>(() => ({ edit: editRef, target: editTarget }), []);
  const provideEditor = useCallback<ProvideEditorCallback<GridCell>>((cell) => {
    const editing = editRef.current;
    // Glide reports the overlay's location in grid columns (row markers included).
    const location = cell.location ? dataCellOf(cell.location) : null;
    if (!editing || !location) return undefined;
    if (!editing.canEdit(location[1], location[0])) return undefined;
    editTarget.current = location;
    return { editor: OverlayEditor, disablePadding: true };
  }, []);

  const selectRef = useRef(onSelectRows);
  selectRef.current = onSelectRows;
  /** The selection last reported to (or received from) the caller; unset so a remount scrolls. */
  const reported = useRef<GridRowSelection | undefined>(undefined);
  const onGridSelectionChange = useCallback((next: GridSelection) => {
    setSelection(next);
    const notify = selectRef.current;
    if (!notify) return;
    const rows = fromGridSelection(next);
    reported.current = rows;
    notify(rows);
  }, []);

  // A selection made by the caller: show it and bring its current row into view.
  useEffect(() => {
    if (!selectedRows || selectedRows === reported.current) return;
    reported.current = selectedRows;
    setSelection((prev) => {
      const col = prev.current?.cell[0] ?? 0;
      return toGridSelection(selectedRows, col);
    });
    if (selectedRows.current !== null) {
      gridRef.current?.scrollTo(0, selectedRows.current, "vertical");
    }
  }, [selectedRows]);

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

  /** The whole value of a data cell: text, `null` (NULL) or `undefined` (not loaded). */
  const valueAt = useCallback(
    (col: number, row: number): string | null | undefined => {
      const override = edit?.override(row, col);
      if (override) return override.placeholder ?? override.text;
      const ref = pager.cell(row, col);
      if (!ref) return undefined;
      const value = formatCell(ref.vector, ref.index, columns[col]);
      return value.isNull ? null : value.text;
    },
    [edit, pager, columns],
  );

  const onCellActivated = useCallback(
    ([col, row]: Item) => {
      if (edit?.canEdit(row, col)) return;
      setValueOpen(true);
    },
    [edit, setValueOpen],
  );

  const currentCell = selection.current?.cell;
  const panelColumn = currentCell ? (columns[currentCell[0]] ?? null) : null;
  const panelValue =
    currentCell && panelColumn ? valueAt(currentCell[0], currentCell[1]) : undefined;

  const columnSummary = columns.map((c) => `${c.name} (${columnTypeLabel(c)})`).join(", ");

  return (
    <div
      className={`result-grid${valueOpen ? " result-grid--with-value" : ""}`}
      data-testid="result-grid"
    >
      <div className="result-grid__main">
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
            onCellActivated={onCellActivated}
            gridSelection={selection}
            onGridSelectionChange={onGridSelectionChange}
            {...(edit
              ? {
                  provideEditor,
                  onDelete: () => false,
                  onPaste: false,
                }
              : {})}
            keybindings={GRID_KEYS}
            rowMarkers="number"
            rowHeight={ROW_HEIGHT}
            headerHeight={HEADER_HEIGHT}
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
      {valueOpen ? (
        <ValuePanel
          column={panelColumn}
          value={panelValue}
          engine={engine}
          onClose={() => {
            setValueOpen(false);
            gridRef.current?.focus();
          }}
        />
      ) : null}
    </div>
  );
}
