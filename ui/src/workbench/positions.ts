/**
 * Editor positions. The core reports statement positions as 1-based lines and 1-based UTF-16
 * columns (`StatementSpan`), which is how Monaco counts and how JS strings index, so a span maps
 * to a string slice without any byte arithmetic in the UI.
 */
import type { StatementSpan } from "../api";

/** The UTF-16 offset of 1-based `line` / `column` in `text`, clamped to the text. */
export function offsetAt(text: string, line: number, column: number): number {
  let offset = 0;
  for (let current = 1; current < line; current += 1) {
    const next = text.indexOf("\n", offset);
    if (next < 0) return text.length;
    offset = next + 1;
  }
  return Math.min(offset + Math.max(column - 1, 0), text.length);
}

/** `[start, end)` UTF-16 offsets of a statement in the script it was split from. */
export function spanOffsets(text: string, span: StatementSpan): [number, number] {
  return [
    offsetAt(text, span.startLine, span.startColumn),
    offsetAt(text, span.endLine, span.endColumn),
  ];
}

/** The text of a statement in the script it was split from. */
export function spanText(text: string, span: StatementSpan): string {
  const [start, end] = spanOffsets(text, span);
  return text.slice(start, end);
}

/** A one-line preview of SQL text for lists and messages. */
export function sqlPreview(sql: string, max = 120): string {
  const flat = sql.replace(/\s+/g, " ").trim();
  return flat.length > max ? `${flat.slice(0, max - 1)}…` : flat;
}
