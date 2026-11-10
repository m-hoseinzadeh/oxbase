/**
 * The result grid: Glide Data Grid (canvas, virtualized) over a {@link ResultPager}. The grid
 * asks for cells of the visible rows only; cells whose page is not loaded yet draw as loading
 * placeholders and are redrawn when the page arrives.
 *
 * With `edit` (table data, M3) the grid also shows rows past the stored ones (new rows), cell
 * values that differ from the store (pending or applied edits), highlights changed, new,
 * deleted and failed rows, and opens the caller's typed editor in the cell overlay.
 *
 * Every grid has (1.1):
 * - a cell menu (right-click): view or edit the value in a dialog, copy, copy with column
 *   names, copy as JSON / Markdown / SQL INSERT / IN list / column names, plus the caller's
 *   items (`cellMenu`); a header menu: fit the column or all columns, copy its name, plus
 *   `headerMenu`;
 * - keys: Enter / Space / F2 activate a cell (edit it, or open the value panel), Shift+Enter
 *   opens the value dialog, Ctrl+Shift+C copies with column names, Ctrl+F searches; Delete,
 *   Ctrl+D and Ctrl+Z go to the caller (`onKey`), Ctrl+V pastes into cells and Ctrl+Shift+V
 *   pastes new rows (`onPasteCells`, `onPasteRows`);
 * - Ctrl/Cmd+click adds cell ranges or rows to the selection, Shift+click extends it;
 * - a preview of a cut-off value when the pointer rests on its cell.
 *
 * With `links` (table data, 1.5) the cells of the given columns (foreign keys) draw a link
 * icon (./cellLinks); clicking it calls `links.open` with the cell.
 */
import {
  type CellClickedEventArgs,
  CompactSelection,
  DataEditor,
  type DataEditorRef,
  type DrawCellCallback,
  type DrawHeaderCallback,
  type GridCell,
  GridCellKind,
  type GridColumn,
  GridColumnIcon,
  type GridKeyEventArgs,
  type GridMouseEventArgs,
  type GridSelection,
  type Item,
  type ProvideEditorCallback,
  type ProvideEditorComponent,
  type Rectangle,
  type Theme,
} from "@glideapps/glide-data-grid";
import {
  Braces,
  ClipboardCopy,
  ClipboardList,
  Columns3,
  Copy,
  Eye,
  FileCode,
  List,
  MoveHorizontal,
  Pencil,
  Sheet,
} from "lucide-react";
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
import { ContextMenu, type MenuItem, OPENS_SUBMENU } from "../../components/ContextMenu";
import { ROW_HEIGHTS, useSettings } from "../../settings";
import { normalizeColor, subscribeTheme } from "../theme-bridge";
import { drawLinkIcon, hitsLinkIcon } from "./cellLinks";
import {
  type CopiedText,
  type CopyNotify,
  cellsForSelection,
  copiedSelection,
  copyColumn,
  NULL_COPY_TEXT,
  readSelection,
} from "./copy";
import {
  type CellText,
  parseTsv,
  quoteIdent,
  toInList,
  toJson,
  toMarkdown,
  toSqlInsert,
  toTsv,
} from "./copyFormats";
import {
  cellDisplayText,
  columnTypeLabel,
  formatCell,
  initialColumnWidth,
  visibleColumns,
} from "./format";
import { dataCellOf } from "./gridCoords";
import type { ResultPager } from "./pager";
import {
  fromGridSelection,
  type GridRowSelection,
  inSelection,
  selectionRects,
  toGridSelection,
} from "./rowSelection";
import { ValueDialog, type ValueDialogEdit } from "./ValueDialog";
import { ValuePanel } from "./ValuePanel";

/**
 * The grid's own keys, changed: no Ctrl+Enter ("scroll to the selected cell"), which runs the
 * query wherever the focus is in the tab (../shortcuts); F2 activates a cell like Enter and
 * Space, while Shift+Enter opens the value dialog; Ctrl+F searches. No copy: Glide writes the
 * cell text into the clipboard's HTML table unescaped, so Oxbase copies itself
 * (`copiedSelection`).
 */
const GRID_KEYS = {
  scrollToSelectedCell: false,
  activateCell: "Enter| |F2",
  search: true,
  copy: false,
} as const;

/** Whether `element` takes typed text (a copy there copies its own selection). */
function takesText(element: Element): boolean {
  return (
    element instanceof HTMLInputElement ||
    element instanceof HTMLTextAreaElement ||
    (element instanceof HTMLElement && element.isContentEditable)
  );
}

/** Writes both flavours of a copy with the async clipboard API (plain text where it lacks
 * `write`). */
async function writeCopied(copied: CopiedText): Promise<void> {
  if (typeof ClipboardItem !== "undefined" && navigator.clipboard.write !== undefined) {
    await navigator.clipboard.write([
      new ClipboardItem({
        "text/plain": new Blob([copied.text], { type: "text/plain" }),
        "text/html": new Blob([copied.html], { type: "text/html" }),
      }),
    ]);
  } else {
    await navigator.clipboard.writeText(copied.text);
  }
}

/** How long the pointer rests on a cut-off cell before its value shows. */
const PREVIEW_DELAY_MS = 600;
/** Most characters the hover preview shows. */
const PREVIEW_CHARS = 1500;
/** Average width of a character of the grid's monospace font (12.5 px). */
const CHAR_WIDTH = 7.6;

/** Rows assumed visible before the grid reports its real viewport. */
const MIN_VIEWPORT_ROWS = 60;
/** Dense grid metrics (docs/design-system.md: 4 px grid); the row height is a setting. */
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
  /** Editing the cell in the value dialog; `undefined` when it cannot be edited. */
  valueEdit?(row: number, col: number): ValueDialogEdit | undefined;
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

/** A cell's link icon was clicked (see `ResultGridProps.links`). */
export interface GridLinkTarget {
  row: number;
  col: number;
  /** The whole value: text, `null` (NULL) or `undefined` (not loaded). */
  value: string | null | undefined;
}

/** Link icons in the cells of some columns (foreign keys of table data). */
export interface GridLinks {
  /** The columns whose cells show the icon. */
  columns: ReadonlySet<number>;
  open: (target: GridLinkTarget) => void;
}

/** Keys the grid hands to its caller (table data): mark for deletion, duplicate, revert. */
export type GridKey = "delete" | "duplicate" | "revert";

/** The cell a context menu was opened on. */
export interface GridCellTarget {
  row: number;
  col: number;
  column: ColumnInfo;
  /** The whole value: text, `null` (NULL) or `undefined` (not loaded). */
  value: string | null | undefined;
  /** The values of the selected cells in this column (stored rows, read like a copy). */
  selectedValues: () => Promise<CellText[]>;
}

type CopyKind = "names" | "json" | "markdown" | "sql" | "in" | "columns";

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
  /** The connection's engine, for the value panel's SQL beautifier and "copy as SQL". */
  engine?: Engine | null | undefined;
  /** The table "Copy as SQL INSERT" inserts into, qualified and quoted (else `table_name`). */
  sqlTable?: string | undefined;
  /** The caller's items of the cell menu, after the grid's own. */
  cellMenu?: ((target: GridCellTarget, rows: GridRowSelection) => MenuItem[]) | undefined;
  /** The caller's items of the header menu, after the grid's own. */
  headerMenu?: ((col: number, column: ColumnInfo) => MenuItem[]) | undefined;
  /** Delete, Ctrl+D, Ctrl+Z in the grid; `true` when the caller used the key. */
  onKey?: ((key: GridKey) => boolean) | undefined;
  /** Ctrl+V: cells pasted at `target` ([col, row]), row by row. */
  onPasteCells?: ((target: Item, values: readonly (readonly string[])[]) => void) | undefined;
  /** Ctrl+Shift+V or "Paste as new rows": each line becomes a new row from column `col`. */
  onPasteRows?: ((values: string[][], col: number) => void) | undefined;
  /** 1.5: link icons in these columns' cells (foreign keys); see the module docs. */
  links?: GridLinks | undefined;
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
  sqlTable,
  cellMenu,
  headerMenu,
  onKey,
  onPasteCells,
  onPasteRows,
  links,
}: ResultGridProps) {
  // The document column is always last, so the shown columns keep their page indexes.
  const columns = useMemo(() => visibleColumns(resultColumns), [resultColumns]);
  const gridRef = useRef<DataEditorRef>(null);
  const portalRef = useRef<HTMLDivElement>(null);
  const mainRef = useRef<HTMLDivElement>(null);
  const region = useRef<Rectangle>({ x: 0, y: 0, width: 0, height: 0 });
  const theme = useGridTheme();
  // Grid rows: compact, default or comfortable (the Settings window).
  const { gridDensity } = useSettings();
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
      if (hoverTimer.current) clearTimeout(hoverTimer.current);
      setHover(null);
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

  // Foreign-key cells: the link icon after the value (before a right-aligned one).
  const linkColumns = links?.columns;
  const linkable = useCallback(
    (cell: GridCell, col: number) =>
      linkColumns?.has(col) === true &&
      cell.kind === GridCellKind.Text &&
      cell.style !== "faded" &&
      !(cell.copyData === NULL_COPY_TEXT && cell.displayData === "NULL"),
    [linkColumns],
  );
  const drawCell = useCallback<DrawCellCallback>(
    (args, drawContent) => {
      drawContent();
      const { cell, col, ctx, rect, theme: t } = args;
      if (!linkable(cell, col)) return;
      const alignRight = cell.kind === GridCellKind.Text && cell.contentAlign === "right";
      drawLinkIcon(ctx, rect, alignRight, t.accentColor, t.bgCell);
    },
    [linkable],
  );
  const linksRef = useRef(links);
  linksRef.current = links;
  const onCellClicked = useCallback(
    ([col, row]: Item, event: CellClickedEventArgs) => {
      const open = linksRef.current?.open;
      if (!open || event.isTouch || event.button !== 0) return;
      if (event.shiftKey || event.ctrlKey || event.metaKey) return;
      const cell = getCellContent([col, row]);
      if (!linkable(cell, col)) return;
      const alignRight = cell.kind === GridCellKind.Text && cell.contentAlign === "right";
      const { width, height } = event.bounds;
      if (!hitsLinkIcon(event.localEventX, event.localEventY, width, height, alignRight)) return;
      event.preventDefault();
      open({ row, col, value: valueAt(col, row) });
    },
    [getCellContent, linkable, valueAt],
  );

  /** The raw bytes of a stored binary cell (an image shows as the image), else `null`. */
  const bytesAt = useCallback(
    (col: number, row: number): Uint8Array | null => {
      if (edit?.override(row, col)) return null;
      const ref = pager.cell(row, col);
      if (!ref) return null;
      const value = ref.vector.get(ref.index) as unknown;
      return value instanceof Uint8Array ? value : null;
    },
    [edit, pager],
  );

  const onCellActivated = useCallback(
    ([col, row]: Item) => {
      if (edit?.canEdit(row, col)) return;
      setValueOpen(true);
    },
    [edit, setValueOpen],
  );

  // --- value dialog, copy as, menus, keys, hover preview (1.1) ----------------------------

  const [dialog, setDialog] = useState<{ col: number; row: number; edit: boolean } | null>(null);
  const [menu, setMenu] = useState<{
    x: number;
    y: number;
    label: string;
    items: MenuItem[];
  } | null>(null);
  const [hover, setHover] = useState<{ text: string; x: number; y: number } | null>(null);
  const hoverTimer = useRef<ReturnType<typeof setTimeout> | null>(null);
  const hideHover = useCallback(() => {
    if (hoverTimer.current) clearTimeout(hoverTimer.current);
    hoverTimer.current = null;
    setHover(null);
  }, []);
  useEffect(() => hideHover, [hideHover]);

  const notify = useCallback((message: string) => noticeRef.current?.(message), []);

  /**
   * Plain Copy (Ctrl+C, the cell menu's Copy): the selection as tab-separated text and an
   * escaped HTML table. In a `copy` event whose cells are all cached it fills the event's
   * clipboard at once; otherwise it reads them first and writes with the clipboard API.
   */
  function copySelection(event?: ClipboardEvent) {
    const rects = selectionRects(selection, columns.length, rowCount);
    if (rects.length === 0) return;
    const copied = copiedSelection(pager, columns, rowCount, rects, notify);
    if (!(copied instanceof Promise) && event?.clipboardData) {
      event.clipboardData.setData("text/plain", copied.text);
      event.clipboardData.setData("text/html", copied.html);
      return;
    }
    void (async () => {
      let result: CopiedText;
      try {
        result = await copied;
      } catch {
        // `copiedSelection` said why (copy failed / too many rows).
        return;
      }
      try {
        await writeCopied(result);
      } catch (error) {
        notify(`Copy failed: ${error instanceof Error ? error.message : String(error)}`);
      }
    })();
  }

  // Ctrl+C (and the system's Copy command) while the grid has the focus; a text field in it
  // (the cell editor, the search box) copies its own text.
  const copyRef = useRef(copySelection);
  copyRef.current = copySelection;
  useEffect(() => {
    const onCopy = (event: ClipboardEvent) => {
      const active = document.activeElement;
      if (!active || !mainRef.current?.contains(active) || takesText(active)) return;
      if (portalRef.current?.contains(active)) return;
      event.preventDefault();
      copyRef.current(event);
    };
    window.addEventListener("copy", onCopy);
    return () => window.removeEventListener("copy", onCopy);
  }, []);

  /** Copies the selection in a "copy as" format (stored rows, read like a copy). */
  async function copyAs(kind: CopyKind) {
    const rects = selectionRects(selection, columns.length, rowCount);
    const first = rects[0];
    if (!first) return;
    const cols = columns.slice(first.x, first.x + first.width).map(copyColumn);
    let rows: CellText[][] = [];
    if (kind !== "columns") {
      try {
        // Ranges over the same columns join; another column span keeps the first one only.
        for (const r of rects.filter((r) => r.x === first.x && r.width === first.width)) {
          rows = rows.concat(await readSelection(pager, columns, rowCount, r, notify));
        }
      } catch {
        // `readSelection` said why (copy failed / too many rows).
        return;
      }
    }
    const table = sqlTable ?? quoteIdent("table_name", engine);
    const numeric = cols.length > 0 && cols.every((c) => c.numeric);
    const text =
      kind === "names"
        ? toTsv(cols, rows, true)
        : kind === "json"
          ? toJson(cols, rows)
          : kind === "markdown"
            ? toMarkdown(cols, rows)
            : kind === "sql"
              ? toSqlInsert(table, cols, rows, engine)
              : kind === "in"
                ? toInList(rows.flat(), numeric, engine)
                : cols.map((c) => c.name).join(", ");
    try {
      await navigator.clipboard.writeText(text);
    } catch (error) {
      notify(`Copy failed: ${error instanceof Error ? error.message : String(error)}`);
    }
  }

  async function pasteRowsFromClipboard(col: number) {
    if (!onPasteRows) return;
    try {
      const text = await navigator.clipboard.readText();
      const values = parseTsv(text);
      if (values.length > 0) onPasteRows(values, col);
    } catch {
      notify("Could not read the clipboard here: press Ctrl+Shift+V in the grid.");
    }
  }

  const select = (next: GridSelection) => onGridSelectionChange(next);

  function openCellMenu([col, row]: Item, x: number, y: number) {
    hideHover();
    // A right-click outside the selection selects what was clicked first.
    let current = selection;
    if (!inSelection(selection, col, row)) {
      current =
        col < 0
          ? { columns: CompactSelection.empty(), rows: CompactSelection.fromSingleSelection(row) }
          : {
              columns: CompactSelection.empty(),
              rows: CompactSelection.empty(),
              current: {
                cell: [col, row],
                range: { x: col, y: row, width: 1, height: 1 },
                rangeStack: [],
              },
            };
      select(current);
    }
    const target = col < 0 ? 0 : col;
    const column = columns[target];
    if (!column) return;
    const editValue = edit?.canEdit(row, target) ? edit.valueEdit?.(row, target) : undefined;
    const items: MenuItem[] = [
      {
        label: "View value",
        icon: Eye,
        shortcut: "Shift+Enter",
        onSelect: () => setDialog({ col: target, row, edit: false }),
      },
      ...(editValue
        ? [
            {
              label: "Edit value…",
              icon: Pencil,
              onSelect: () => setDialog({ col: target, row, edit: true }),
            },
          ]
        : []),
      {
        label: "Copy",
        icon: Copy,
        shortcut: "Ctrl+C",
        separatorBefore: true,
        onSelect: () => copySelection(),
      },
      {
        label: "Copy with column names",
        icon: ClipboardList,
        shortcut: "Ctrl+Shift+C",
        onSelect: () => void copyAs("names"),
      },
      {
        label: "Copy as",
        icon: ClipboardCopy,
        onSelect: OPENS_SUBMENU,
        submenu: [
          { label: "JSON", icon: Braces, onSelect: () => void copyAs("json") },
          { label: "Markdown table", icon: Sheet, onSelect: () => void copyAs("markdown") },
          { label: "SQL INSERT", icon: FileCode, onSelect: () => void copyAs("sql") },
          { label: "IN list", icon: List, onSelect: () => void copyAs("in") },
          { label: "Column names", icon: Columns3, onSelect: () => void copyAs("columns") },
        ],
      },
      ...(cellMenu?.(
        {
          row,
          col: target,
          column,
          value: valueAt(target, row),
          selectedValues: async () => {
            const values: CellText[] = [];
            for (const r of selectionRects(current, columns.length, rowCount)) {
              if (target < r.x || target >= r.x + r.width) continue;
              const rect = { x: target, y: r.y, width: 1, height: r.height };
              const rows = await readSelection(pager, columns, rowCount, rect, notify);
              for (const line of rows) values.push(line[0] ?? null);
            }
            return values;
          },
        },
        fromGridSelection(current),
      ) ?? []),
    ];
    setMenu({ x, y, label: "Cell", items });
  }

  function openHeaderMenu(col: number, x: number, y: number) {
    hideHover();
    const column = columns[col];
    if (!column) return;
    const items: MenuItem[] = [
      {
        label: "Fit column",
        icon: MoveHorizontal,
        onSelect: () =>
          gridRef.current?.remeasureColumns(CompactSelection.fromSingleSelection(col)),
      },
      {
        label: "Fit all columns",
        icon: Columns3,
        onSelect: () =>
          gridRef.current?.remeasureColumns(
            CompactSelection.fromSingleSelection([0, columns.length]),
          ),
      },
      {
        label: "Copy column name",
        icon: Copy,
        onSelect: () => void navigator.clipboard.writeText(column.name).catch(() => {}),
      },
      ...(headerMenu?.(col, column) ?? []),
    ];
    setMenu({ x, y, label: `Column ${column.name}`, items });
  }

  function onGridKeyDown(event: GridKeyEventArgs) {
    hideHover();
    const mod = event.ctrlKey || event.metaKey;
    const key = event.key.length === 1 ? event.key.toLowerCase() : event.key;
    const cell = selection.current?.cell;
    if (key === "Enter" && event.shiftKey && !mod && !event.altKey) {
      if (!cell) return;
      event.cancel();
      setDialog({ col: cell[0], row: cell[1], edit: false });
      return;
    }
    if (mod && event.shiftKey && key === "c") {
      event.cancel();
      void copyAs("names");
      return;
    }
    if (mod && event.shiftKey && key === "v" && onPasteRows) {
      event.cancel();
      void pasteRowsFromClipboard(cell?.[0] ?? 0);
      return;
    }
    const gridKey: GridKey | null =
      (key === "Delete" && !mod) || (mod && event.altKey && key === "-")
        ? "delete"
        : mod && !event.shiftKey && key === "d"
          ? "duplicate"
          : mod && !event.shiftKey && key === "z"
            ? "revert"
            : null;
    if (gridKey && onKey?.(gridKey)) event.cancel();
  }

  const onItemHovered = useCallback(
    (args: GridMouseEventArgs) => {
      if (hoverTimer.current) clearTimeout(hoverTimer.current);
      hoverTimer.current = null;
      setHover(null);
      if (args.kind !== "cell") return;
      const [col, row] = args.location;
      const shown = gridColumns[col];
      const width = shown && "width" in shown ? shown.width : undefined;
      if (col < 0 || width === undefined) return;
      const { bounds } = args;
      hoverTimer.current = setTimeout(() => {
        const value = valueAt(col, row);
        if (typeof value !== "string") return;
        // Only a value the cell cuts off: a line break, or wider than the column.
        const cut = value.includes("\n") || value.length * CHAR_WIDTH > width - 20;
        if (!cut) return;
        const text = value.length > PREVIEW_CHARS ? `${value.slice(0, PREVIEW_CHARS)}…` : value;
        setHover({ text, x: bounds.x, y: bounds.y + bounds.height + 2 });
      }, PREVIEW_DELAY_MS);
    },
    [gridColumns, valueAt],
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
      <div ref={mainRef} className="result-grid__main">
        <OverlayTarget.Provider value={overlay}>
          <DataEditor
            ref={gridRef}
            columns={gridColumns}
            rows={rowCount + (edit?.extraRows ?? 0)}
            getCellContent={getCellContent}
            drawHeader={drawHeader}
            {...(links ? { drawCell, onCellClicked } : {})}
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
                  onPaste: onPasteCells
                    ? (target: Item, values: readonly (readonly string[])[]) => {
                        onPasteCells(target, values);
                        // The caller applied it (or refused); Glide writes nothing itself.
                        return false;
                      }
                    : false,
                }
              : {})}
            keybindings={GRID_KEYS}
            rangeSelect="multi-rect"
            onKeyDown={onGridKeyDown}
            onItemHovered={onItemHovered}
            onCellContextMenu={(cell, event) => {
              event.preventDefault();
              openCellMenu(
                cell,
                event.bounds.x + event.localEventX,
                event.bounds.y + event.localEventY,
              );
            }}
            onHeaderContextMenu={(col, event) => {
              event.preventDefault();
              openHeaderMenu(
                col,
                event.bounds.x + event.localEventX,
                event.bounds.y + event.localEventY,
              );
            }}
            rowMarkers="number"
            rowHeight={ROW_HEIGHTS[gridDensity]}
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
          bytes={currentCell && panelColumn ? bytesAt(currentCell[0], currentCell[1]) : null}
          engine={engine}
          onClose={() => {
            setValueOpen(false);
            gridRef.current?.focus();
          }}
        />
      ) : null}
      {dialog && columns[dialog.col] ? (
        <ValueDialog
          key={`${dialog.col}:${dialog.row}:${dialog.edit}`}
          column={columns[dialog.col] as ColumnInfo}
          value={valueAt(dialog.col, dialog.row)}
          bytes={bytesAt(dialog.col, dialog.row)}
          engine={engine}
          edit={
            edit?.canEdit(dialog.row, dialog.col)
              ? edit.valueEdit?.(dialog.row, dialog.col)
              : undefined
          }
          startEditing={dialog.edit}
          onClose={() => {
            setDialog(null);
            gridRef.current?.focus();
          }}
        />
      ) : null}
      {menu ? (
        <ContextMenu
          x={menu.x}
          y={menu.y}
          label={menu.label}
          items={menu.items}
          onClose={() => setMenu(null)}
        />
      ) : null}
      {hover ? (
        <div
          className="cell-preview"
          role="tooltip"
          data-testid="cell-preview"
          style={{
            left: Math.max(4, Math.min(hover.x, window.innerWidth - 484)),
            top: Math.min(hover.y, window.innerHeight - 120),
          }}
        >
          {hover.text}
        </div>
      ) : null}
    </div>
  );
}
