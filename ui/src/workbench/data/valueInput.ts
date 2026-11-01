/**
 * Client-side hints for typed values (the core's `convert` grammar, docs/m3-contracts.md §5).
 * They only warn while typing; the core is the authority and rejects what it cannot parse.
 */
import type { CellInput, DataType, InputKind } from "../../api";

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

/** Whether the column holds XML (edited as text, converted by the server). */
export function isXmlType(dataType: DataType | undefined): boolean {
  return dataType?.type === "other" && dataType.name.toLowerCase() === "xml";
}

/** Bytes of a `0x` hex, `\\x` hex or `base64:` value, or `null` when it does not parse. */
export function byteLength(text: string): number | null {
  const t = text.trim();
  const hex = /^(0x|\\x)([0-9a-f]*)$/i.exec(t);
  if (hex) {
    const digits = hex[2] ?? "";
    return digits.length % 2 === 0 ? digits.length / 2 : null;
  }
  const b64 = /^base64:([A-Za-z0-9+/]*)(={0,2})$/.exec(t);
  if (b64) {
    const body = b64[1] ?? "";
    const pad = (b64[2] ?? "").length;
    if ((body.length + pad) % 4 !== 0) return null;
    return ((body.length + pad) / 4) * 3 - pad;
  }
  return null;
}

/** A problem with an XML fragment (the server accepts several top-level nodes), or `null`. */
function xmlProblem(text: string): string | null {
  if (typeof DOMParser === "undefined") return null;
  const doc = new DOMParser().parseFromString(
    `<oxbase-root>${text}</oxbase-root>`,
    "application/xml",
  );
  return doc.getElementsByTagName("parsererror").length > 0 ? "not well-formed XML" : null;
}

/**
 * What the text should look like for the column's type, beyond the kind's grammar (decimal
 * precision and scale such as `money`'s 4 places, binary lengths, XML), or `null`. Only a hint:
 * the core and the server decide.
 */
export function typedProblem(
  kind: InputKind,
  text: string,
  dataType: DataType | undefined,
): string | null {
  const basic = inputProblem(kind, text);
  if (basic || !dataType) return basic;
  const t = text.trim();
  if (kind.type === "decimal" && dataType.type === "decimal") {
    const m = /^[+-]?(\d*)(?:\.(\d*))?$/.exec(t);
    if (!m) return null;
    const whole = (m[1] ?? "").replace(/^0+/, "");
    const fraction = m[2] ?? "";
    const { precision, scale } = dataType;
    if (scale !== null && fraction.replace(/0+$/, "").length > scale) {
      return `at most ${scale} decimal ${scale === 1 ? "place" : "places"}`;
    }
    if (precision !== null && whole.length > precision - (scale ?? 0)) {
      return `at most ${precision - (scale ?? 0)} digits before the point`;
    }
    return null;
  }
  if (kind.type === "bytes") {
    const n = byteLength(t);
    if (n === null) return "expected an even number of hex digits or valid base64";
    const max =
      dataType.type === "binary" || dataType.type === "var_binary" ? dataType.length : null;
    if (max !== null && n > max) return `${n} bytes: at most ${max} fit`;
    return null;
  }
  if (isXmlType(dataType)) return xmlProblem(text);
  return null;
}

/** A new random UUID (version 4), for the "New" button of UUID columns. */
export function newUuid(): string {
  if (typeof crypto.randomUUID === "function") return crypto.randomUUID();
  const b = crypto.getRandomValues(new Uint8Array(16));
  b[6] = ((b[6] ?? 0) & 0x0f) | 0x40;
  b[8] = ((b[8] ?? 0) & 0x3f) | 0x80;
  const hex = Array.from(b, (x) => x.toString(16).padStart(2, "0")).join("");
  return `${hex.slice(0, 8)}-${hex.slice(8, 12)}-${hex.slice(12, 16)}-${hex.slice(16, 20)}-${hex.slice(20)}`;
}
