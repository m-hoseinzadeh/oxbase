/**
 * The key browser's key list: the rows of a key scan (`key`, `key_text`, `type`, `ttl_ms`),
 * paged from the core like a grid (only the visible rows are loaded) and drawn as a listbox
 * with a type badge, the key, its TTL and (once read) its size.
 */
import { type KeyboardEvent, useEffect, useRef, useState } from "react";
import type { BinaryText, ColumnInfo, KeyInfo, KeyType } from "../../../api";
import type { ResultPager } from "../../results/pager";
import {
  asKeyType,
  cellBinary,
  cellValue,
  KEY_TYPE_BADGE,
  KEY_TYPE_LABEL,
  sizeText,
  ttlText,
} from "./keyModel";

export const KEY_ROW_HEIGHT = 28;
const OVERSCAN = 8;

/** A loaded row of the scan. */
export interface KeyRow {
  key: BinaryText;
  text: string;
  type: KeyType;
  ttlMs: number | null;
}

/** Row `row` of the scan, or `null` while its page is not loaded. */
export function keyRowAt(pager: ResultPager, columns: ColumnInfo[], row: number): KeyRow | null {
  const index = (name: string) => columns.findIndex((c) => c.name === name);
  const keyCol = index("key");
  const ref = pager.cell(row, Math.max(keyCol, 0));
  if (!ref) return null;
  const at = (col: number) => {
    if (col < 0) return null;
    const cell = pager.cell(row, col);
    return cell ? cellValue(cell.vector, cell.index) : null;
  };
  const key = cellBinary(cellValue(ref.vector, ref.index));
  if (!key) return null;
  const text = at(index("key_text"));
  const ttl = at(index("ttl_ms"));
  return {
    key,
    text: typeof text === "string" ? text : key.encoding === "utf8" ? key.text : key.hex,
    type: asKeyType(at(index("type")) as string | null),
    ttlMs: ttl === null || ttl === undefined ? null : Number(ttl),
  };
}

export interface KeyListProps {
  pager: ResultPager;
  columns: ColumnInfo[];
  rowCount: number;
  selected: number | null;
  onSelect: (row: number, key: KeyRow) => void;
  /** Sizes read so far, by key text. */
  sizes: ReadonlyMap<string, KeyInfo | "error">;
  /** The rows on screen (to read their sizes). */
  onVisible?: ((rows: KeyRow[]) => void) | undefined;
}

export function KeyList({
  pager,
  columns,
  rowCount,
  selected,
  onSelect,
  sizes,
  onVisible,
}: KeyListProps) {
  const scroller = useRef<HTMLDivElement>(null);
  const [scrollTop, setScrollTop] = useState(0);
  const [height, setHeight] = useState(600);
  const [, setTick] = useState(0);

  useEffect(() => {
    pager.setRowCount(rowCount);
  }, [pager, rowCount]);
  useEffect(() => pager.onLoad(() => setTick((t) => t + 1)), [pager]);
  useEffect(() => {
    const el = scroller.current;
    if (!el || typeof ResizeObserver === "undefined") return;
    const observer = new ResizeObserver(() => setHeight(el.clientHeight || 600));
    observer.observe(el);
    return () => observer.disconnect();
  }, []);

  const first = Math.max(0, Math.floor(scrollTop / KEY_ROW_HEIGHT) - OVERSCAN);
  const last = Math.min(rowCount - 1, Math.ceil((scrollTop + height) / KEY_ROW_HEIGHT) + OVERSCAN);
  useEffect(() => {
    if (last >= first) pager.setVisible(first, last);
  }, [pager, first, last]);

  const rows: { row: number; key: KeyRow | null }[] = [];
  for (let row = first; row <= last; row += 1)
    rows.push({ row, key: keyRowAt(pager, columns, row) });
  const loadedKeys = rows.flatMap((r) => (r.key ? [r.key] : []));
  const visibleSignature = loadedKeys.map((k) => k.text).join("\u0000");
  const visibleRef = useRef(onVisible);
  visibleRef.current = onVisible;
  const loadedRef = useRef(loadedKeys);
  loadedRef.current = loadedKeys;
  useEffect(() => {
    void visibleSignature;
    const timer = setTimeout(() => visibleRef.current?.(loadedRef.current), 150);
    return () => clearTimeout(timer);
  }, [visibleSignature]);

  function select(row: number) {
    const key = keyRowAt(pager, columns, row);
    if (key) onSelect(row, key);
    const el = scroller.current;
    if (!el) return;
    const top = row * KEY_ROW_HEIGHT;
    if (top < el.scrollTop) el.scrollTop = top;
    else if (top + KEY_ROW_HEIGHT > el.scrollTop + el.clientHeight) {
      el.scrollTop = top + KEY_ROW_HEIGHT - el.clientHeight;
    }
  }

  function onKeyDown(event: KeyboardEvent<HTMLDivElement>) {
    if (rowCount === 0) return;
    const at = selected ?? -1;
    let next: number | null = null;
    if (event.key === "ArrowDown") next = Math.min(at + 1, rowCount - 1);
    else if (event.key === "ArrowUp") next = Math.max(at - 1, 0);
    else if (event.key === "Home") next = 0;
    else if (event.key === "End") next = rowCount - 1;
    else if (event.key === "PageDown")
      next = Math.min(at + Math.floor(height / KEY_ROW_HEIGHT), rowCount - 1);
    else if (event.key === "PageUp") next = Math.max(at - Math.floor(height / KEY_ROW_HEIGHT), 0);
    if (next === null) return;
    event.preventDefault();
    select(next);
  }

  return (
    <div
      ref={scroller}
      className="key-list"
      role="listbox"
      aria-label="Keys"
      tabIndex={0}
      data-testid="key-list"
      aria-activedescendant={selected !== null ? `key-row-${selected}` : undefined}
      onKeyDown={onKeyDown}
      onScroll={(e) => setScrollTop(e.currentTarget.scrollTop)}
    >
      <div className="key-list__spacer" style={{ height: `${rowCount * KEY_ROW_HEIGHT}px` }}>
        {rows.map(({ row, key }) => {
          const info = key ? sizes.get(key.text) : undefined;
          return (
            // biome-ignore lint/a11y/useKeyWithClickEvents: the listbox handles the keyboard.
            <div
              key={row}
              id={`key-row-${row}`}
              role="option"
              tabIndex={-1}
              aria-selected={row === selected}
              data-testid="key-row"
              className="key-list__row"
              style={{ top: `${row * KEY_ROW_HEIGHT}px`, height: `${KEY_ROW_HEIGHT}px` }}
              onClick={() => select(row)}
            >
              {key ? (
                <>
                  <span
                    className={`key-type key-type--${key.type}`}
                    title={KEY_TYPE_LABEL[key.type]}
                  >
                    {KEY_TYPE_BADGE[key.type]}
                    <span className="visually-hidden"> ({KEY_TYPE_LABEL[key.type]})</span>
                  </span>
                  <span className="key-list__name" title={key.text}>
                    {key.text}
                  </span>
                  <span className="key-list__size">
                    {info && info !== "error" ? sizeText(info) : ""}
                  </span>
                  <span
                    className={`key-list__ttl${key.ttlMs === null ? " key-list__ttl--none" : ""}`}
                    title={key.ttlMs === null ? "No expiry" : `Expires in ${ttlText(key.ttlMs)}`}
                  >
                    {key.ttlMs === null ? "∞" : ttlText(key.ttlMs)}
                  </span>
                </>
              ) : (
                <span className="key-list__loading skeleton" aria-hidden="true" />
              )}
            </div>
          );
        })}
      </div>
    </div>
  );
}
