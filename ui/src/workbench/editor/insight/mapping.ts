/**
 * Pure conversions between the insight payloads (docs/m6-contracts.md §7: UTF-16 offsets) and
 * Monaco's shapes (line / column positions, enum values). Kept free of the `monaco-editor`
 * import so tests run without a browser: callers pass the enum values in {@link MonacoEnums}.
 */
import type {
  CompletionItem,
  CompletionKind,
  DiagnosticItem,
  HoverResponse,
  Severity,
  Utf16Range,
} from "../../../api";

/** 1-based line and column, as Monaco's `IPosition`. */
export interface PositionLike {
  lineNumber: number;
  column: number;
}

/** Monaco's `IRange`. */
export interface RangeLike {
  startLineNumber: number;
  startColumn: number;
  endLineNumber: number;
  endColumn: number;
}

/** What the insight code reads from a Monaco text model. */
export interface ModelLike {
  getValue(): string;
  getOffsetAt(position: PositionLike): number;
  getPositionAt(offset: number): PositionLike;
  getVersionId(): number;
}

/** The Monaco enum values the mapping needs (`monaco.languages.*`, `monaco.MarkerSeverity`). */
export interface MonacoEnums {
  CompletionItemKind: {
    Method: number;
    Function: number;
    Field: number;
    Variable: number;
    Struct: number;
    Interface: number;
    Module: number;
    Value: number;
    Keyword: number;
    Folder: number;
    Reference: number;
    TypeParameter: number;
    Snippet: number;
  };
  CompletionItemInsertTextRule: { InsertAsSnippet: number };
  MarkerSeverity: { Hint: number; Warning: number; Error: number };
}

/** A Monaco `CompletionItem` (the fields Oxbase fills). */
export interface SuggestionLike {
  label: string | { label: string; description?: string };
  kind: number;
  insertText: string;
  insertTextRules?: number;
  detail?: string;
  documentation?: { value: string };
  sortText: string;
  filterText?: string;
  range: { insert: RangeLike; replace: RangeLike };
}

/** A Monaco `IMarkerData`. */
export interface MarkerLike extends RangeLike {
  severity: number;
  message: string;
  code: string;
  source: string;
}

/** The marker owner and source name of insight diagnostics. */
export const MARKER_OWNER = "oxbase-insight";

/** `kind` → Monaco `CompletionItemKind` (its codicon tells tables, columns, ... apart). */
export function completionKind(kind: CompletionKind, enums: MonacoEnums): number {
  const k = enums.CompletionItemKind;
  switch (kind) {
    case "keyword":
      return k.Keyword;
    case "database":
      return k.Module;
    case "schema":
      return k.Folder;
    case "table":
      return k.Struct;
    case "view":
      return k.Interface;
    case "column":
      return k.Field;
    case "function":
      return k.Function;
    case "procedure":
      return k.Method;
    case "type":
      return k.TypeParameter;
    case "sequence":
      return k.Value;
    case "alias":
      return k.Variable;
    case "snippet":
      return k.Snippet;
    case "join":
      return k.Reference;
  }
}

/** `severity` → Monaco `MarkerSeverity`. */
export function markerSeverity(severity: Severity, enums: MonacoEnums): number {
  switch (severity) {
    case "error":
      return enums.MarkerSeverity.Error;
    case "warning":
      return enums.MarkerSeverity.Warning;
    case "hint":
      return enums.MarkerSeverity.Hint;
  }
}

function clamp(value: number, max: number): number {
  return Math.max(0, Math.min(value, max));
}

/** The Monaco range of UTF-16 `range` in `model` (clamped to the text). */
export function toRange(model: ModelLike, range: Utf16Range, length?: number): RangeLike {
  const max = length ?? model.getValue().length;
  const start = clamp(Math.min(range.start, range.end), max);
  const end = clamp(Math.max(range.start, range.end), max);
  const a = model.getPositionAt(start);
  const b = model.getPositionAt(end);
  return {
    startLineNumber: a.lineNumber,
    startColumn: a.column,
    endLineNumber: b.lineNumber,
    endColumn: b.column,
  };
}

/**
 * Line / column positions of one text as it was when a request was sent (Monaco reads a
 * completion's ranges against the text at the request's position, even when the user typed on
 * meanwhile). Lines end at `\r\n`, `\r` or `\n`, as in Monaco; columns count UTF-16 units.
 */
export class TextPositions {
  private readonly starts: number[] = [0];

  constructor(readonly text: string) {
    for (let i = 0; i < text.length; i += 1) {
      const c = text.charCodeAt(i);
      if (c === 13 && text.charCodeAt(i + 1) === 10) {
        i += 1;
        this.starts.push(i + 1);
      } else if (c === 10 || c === 13) {
        this.starts.push(i + 1);
      }
    }
  }

  /** The 1-based position of UTF-16 `offset` (clamped to the text). */
  positionAt(offset: number): PositionLike {
    const at = clamp(offset, this.text.length);
    let lo = 0;
    let hi = this.starts.length - 1;
    while (lo < hi) {
      const mid = (lo + hi + 1) >> 1;
      if ((this.starts[mid] ?? 0) <= at) lo = mid;
      else hi = mid - 1;
    }
    return { lineNumber: lo + 1, column: at - (this.starts[lo] ?? 0) + 1 };
  }
}

/**
 * The insert / replace ranges of an item: Monaco needs both on the cursor's line, the insert
 * range ending at the cursor and the replace range containing it. A range from the core that
 * breaks those rules (it never should) falls back to an empty range at the cursor. Also
 * returns the item's start offset after that check.
 */
function itemRanges(
  positions: TextPositions,
  range: Utf16Range,
  cursor: number,
): { insert: RangeLike; replace: RangeLike; start: number } {
  const length = positions.text.length;
  let start = clamp(range.start, length);
  let end = clamp(range.end, length);
  if (start > cursor || end < cursor) {
    start = cursor;
    end = cursor;
  }
  const at = positions.positionAt(cursor);
  const from = positions.positionAt(start);
  const to = positions.positionAt(end);
  const sameLine = from.lineNumber === at.lineNumber;
  const fromCol = sameLine ? from.column : at.column;
  const toCol = to.lineNumber === at.lineNumber ? to.column : at.column;
  const line = at.lineNumber;
  return {
    insert: {
      startLineNumber: line,
      startColumn: fromCol,
      endLineNumber: line,
      endColumn: at.column,
    },
    replace: { startLineNumber: line, startColumn: fromCol, endLineNumber: line, endColumn: toCol },
    start: sameLine ? start : cursor,
  };
}

/** The characters that open a quoted identifier (the core's word then starts with one). */
const OPENING_QUOTES = ['"', "`", "["];

/**
 * The text Monaco filters an item by. Monaco matches the text from the item's range start to
 * the cursor against `filterText` (else the label); when that text starts with an identifier
 * quote the name lacks (`"cu` against `customers`), nothing would match, so the quote is put
 * in front of the name.
 */
function filterTextOf(item: CompletionItem, typed: string): string | undefined {
  const name = item.filterText || item.label;
  const quote = typed.charAt(0);
  if (OPENING_QUOTES.includes(quote) && !name.startsWith(quote)) return quote + name;
  return item.filterText || undefined;
}

/**
 * Monaco suggestions for `items`, answered for `cursor` (UTF-16) in `text`, the text the
 * request was sent with (a `string`, or its {@link TextPositions} when already built).
 */
export function toSuggestions(
  items: CompletionItem[],
  text: string | TextPositions,
  cursor: number,
  enums: MonacoEnums,
): SuggestionLike[] {
  const positions = typeof text === "string" ? new TextPositions(text) : text;
  return items.map((item) => {
    const { insert, replace, start } = itemRanges(positions, item.range, cursor);
    const suggestion: SuggestionLike = {
      label: item.detail ? { label: item.label, description: item.detail } : item.label,
      kind: completionKind(item.kind, enums),
      insertText: item.insertText,
      sortText: item.sortText,
      range: { insert, replace },
    };
    if (item.isSnippet)
      suggestion.insertTextRules = enums.CompletionItemInsertTextRule.InsertAsSnippet;
    if (item.detail) suggestion.detail = item.detail;
    if (item.documentation) suggestion.documentation = { value: item.documentation };
    const filterText = filterTextOf(item, positions.text.slice(start, cursor));
    if (filterText) suggestion.filterText = filterText;
    return suggestion;
  });
}

/** Monaco markers for `diagnostics` over `model`'s current text. */
export function toMarkers(
  diagnostics: DiagnosticItem[],
  model: ModelLike,
  enums: MonacoEnums,
): MarkerLike[] {
  const length = model.getValue().length;
  return diagnostics.map((d) => ({
    ...toRange(model, d.range, length),
    severity: markerSeverity(d.severity, enums),
    message: d.message,
    code: d.code,
    source: MARKER_OWNER,
  }));
}

/** Monaco hover contents for an answer. */
export function toHover(
  hover: HoverResponse,
  model: ModelLike,
): { contents: { value: string }[]; range: RangeLike } {
  return { contents: [{ value: hover.markdown }], range: toRange(model, hover.range) };
}
