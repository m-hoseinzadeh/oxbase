/**
 * The result grid: Glide Data Grid (canvas, virtualized) over a {@link ResultPager}. The grid
 * asks for cells of the visible rows only; cells whose page is not loaded yet draw as loading
 * placeholders and are redrawn when the page arrives.
 */
import {
  DataEditor,
  type DataEditorRef,
  type DrawHeaderCallback,
  type GridCell,
  GridCellKind,
  type GridColumn,
  GridColumnIcon,
  type Item,
  type Rectangle,
  type Theme,
} from "@glideapps/glide-data-grid";
import { type RefObject, useCallback, useEffect, useMemo, useRef, useState } from "react";
import type { ColumnInfo } from "../../api";
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

export interface ResultGridProps {
  pager: ResultPager;
  columns: ColumnInfo[];
  /** Rows stored so far; grows while the result streams. */
  rowCount: number;
}

export function ResultGrid({ pager, columns, rowCount }: ResultGridProps) {
  const gridRef = useRef<DataEditorRef>(null);
  const portalRef = useRef<HTMLDivElement>(null);
  const region = useRef<Rectangle>({ x: 0, y: 0, width: 0, height: 0 });
  const theme = useGridTheme();
  const [widths, setWidths] = useState<Record<number, number>>({});

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

  const getCellContent = useCallback(
    ([col, row]: Item): GridCell => {
      const ref = pager.cell(row, col);
      if (!ref) return LOADING;
      const value = formatCell(ref.vector, ref.index, columns[col]);
      if (value.isNull) {
        return {
          kind: GridCellKind.Text,
          data: "",
          displayData: "NULL",
          copyData: "",
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
    <div className="result-grid">
      <DataEditor
        ref={gridRef}
        columns={gridColumns}
        rows={rowCount}
        getCellContent={getCellContent}
        drawHeader={drawHeader}
        onVisibleRegionChanged={onVisibleRegionChanged}
        onColumnResize={(_column, newSize, index) =>
          setWidths((current) => ({ ...current, [index]: newSize }))
        }
        getCellsForSelection
        rowMarkers="number"
        smoothScrollX
        smoothScrollY
        theme={theme}
        width="100%"
        height="100%"
        portalElementRef={portalRef as RefObject<HTMLElement>}
      />
      <div ref={portalRef} className="result-grid__portal" />
      <span className="visually-hidden">Columns: {columnSummary}</span>
    </div>
  );
}
