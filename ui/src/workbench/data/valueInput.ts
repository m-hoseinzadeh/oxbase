/**
 * Client-side hints for typed values (the core's `convert` grammar, docs/m3-contracts.md §5).
 * They only warn while typing; the core is the authority and rejects what it cannot parse.
 */
import type { CellInput, InputKind } from "../../api";

const PATTERNS: Partial<Record<InputKind["type"], { re: RegExp; hint: string }>> = {
  integer: { re: /^[+-]?\d+$/, hint: "a whole number" },
  decimal: { re: /^[+-]?(\d+\.?\d*|\.\d+)([eE][+-]?\d+)?$/, hint: "a number like 12.50" },
  float: {
    re: /^([+-]?(\d+\.?\d*|\.\d+)([eE][+-]?\d+)?|NaN|[+-]?Infinity)$/,
    hint: "a number, NaN or ±Infinity",
  },
  boolean: { re: /^(true|false|t|f|1|0|yes|no)$/i, hint: "true or false" },
  date: { re: /^\d{4,}-\d{2}-\d{2}( BC)?$/, hint: "YYYY-MM-DD" },
  time: { re: /^\d{2}:\d{2}(:\d{2}(\.\d{1,9})?)?$/, hint: "HH:MM:SS[.ffffff]" },
  timestamp: {
    re: /^\d{4,}-\d{2}-\d{2}[ T]\d{2}:\d{2}(:\d{2}(\.\d{1,9})?)?( BC)?$/,
    hint: "YYYY-MM-DD HH:MM:SS[.ffffff]",
  },
  timestampTz: {
    re: /^\d{4,}-\d{2}-\d{2}[ T]\d{2}:\d{2}(:\d{2}(\.\d{1,9})?)?(Z|[+-]\d{2}(:?\d{2})?)?$/i,
    hint: "YYYY-MM-DD HH:MM:SS[.ffffff]+HH:MM (no offset = UTC)",
  },
  uuid: {
    re: /^[0-9a-f]{8}-?[0-9a-f]{4}-?[0-9a-f]{4}-?[0-9a-f]{4}-?[0-9a-f]{12}$/i,
    hint: "a UUID",
  },
  bytes: { re: /^(0x[0-9a-f]*|\\x[0-9a-f]*|base64:[A-Za-z0-9+/=]*)$/i, hint: "0x hex or base64:" },
};

/** What the text should look like, when it does not; `null` when it looks fine. */
export function inputProblem(kind: InputKind, text: string): string | null {
  if (kind.type === "json") {
    try {
      JSON.parse(text);
      return null;
    } catch {
      return "not valid JSON";
    }
  }
  if (kind.type === "enum") {
    return kind.labels.length === 0 || kind.labels.includes(text)
      ? null
      : `one of ${kind.labels.join(", ")}`;
  }
  const pattern = PATTERNS[kind.type];
  if (!pattern || pattern.re.test(text.trim())) return null;
  return `expected ${pattern.hint}`;
}

/** A placeholder for an empty input of this kind. */
export function placeholderFor(kind: InputKind): string {
  return PATTERNS[kind.type]?.hint ?? "";
}

function pad(n: number, width = 2): string {
  return String(n).padStart(width, "0");
}

/** The current local date / time in the grammar of `kind` (for the "Now" button). */
export function nowText(kind: InputKind, at = new Date()): string | null {
  const date = `${pad(at.getFullYear(), 4)}-${pad(at.getMonth() + 1)}-${pad(at.getDate())}`;
  const time = `${pad(at.getHours())}:${pad(at.getMinutes())}:${pad(at.getSeconds())}`;
  switch (kind.type) {
    case "date":
      return date;
    case "time":
      return time;
    case "timestamp":
      return `${date} ${time}`;
    case "timestampTz":
      return at
        .toISOString()
        .replace("T", " ")
        .replace(/\.\d+Z$/, "Z");
    default:
      return null;
  }
}

/** Pretty-prints JSON text (unchanged when it does not parse). */
export function formatJson(text: string): string {
  try {
    return JSON.stringify(JSON.parse(text), null, 2);
  } catch {
    return text;
  }
}

/** How a cell input reads in the grid and the form. */
export function inputLabel(input: CellInput): string {
  switch (input.type) {
    case "null":
      return "NULL";
    case "default":
      return "DEFAULT";
    case "text":
      return input.text;
  }
}

export function sameInput(a: CellInput, b: CellInput): boolean {
  if (a.type !== b.type) return false;
  return a.type !== "text" || a.text === (b as { text: string }).text;
}
