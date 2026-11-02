/**
 * The key browser's model (Redis, docs/m7-contracts.md §3.2, §5.2): labels, TTL and size text,
 * reading key and value cells from Arrow pages, and the `KeyEdit`s each typed editor sends.
 * Pure functions, so they are easy to test.
 */
import type { Vector } from "apache-arrow";
import {
  type BinaryText,
  binaryText,
  displayBinary,
  type KeyEdit,
  type KeyInfo,
  type KeyType,
  utf8,
} from "../../../api";

/** Types a key can be created with (and filtered by). */
export const KEY_TYPES: readonly Exclude<KeyType, "other">[] = [
  "string",
  "hash",
  "list",
  "set",
  "zset",
  "stream",
  "json",
];

export const KEY_TYPE_LABEL: Record<KeyType, string> = {
  string: "String",
  hash: "Hash",
  list: "List",
  set: "Set",
  zset: "Sorted set",
  stream: "Stream",
  json: "JSON",
  other: "Other",
};

/** Short badge text per type. */
export const KEY_TYPE_BADGE: Record<KeyType, string> = {
  string: "STR",
  hash: "HASH",
  list: "LIST",
  set: "SET",
  zset: "ZSET",
  stream: "STRM",
  json: "JSON",
  other: "?",
};

export function asKeyType(text: string | null | undefined): KeyType {
  return text && text in KEY_TYPE_LABEL ? (text as KeyType) : "other";
}

/** A TTL in milliseconds as text: `no expiry`, `45 s`, `12 min`, `3 h 5 min`, `2 d 4 h`. */
export function ttlText(ms: number | null | undefined): string {
  if (ms === null || ms === undefined || ms < 0) return "no expiry";
  const s = Math.floor(ms / 1000);
  if (s < 1) return `${ms} ms`;
  if (s < 60) return `${s} s`;
  const min = Math.floor(s / 60);
  if (min < 60) return `${min} min`;
  const h = Math.floor(min / 60);
  if (h < 24) return min % 60 ? `${h} h ${min % 60} min` : `${h} h`;
  const d = Math.floor(h / 24);
  return h % 24 ? `${d} d ${h % 24} h` : `${d} d`;
}

export function bytesText(n: number): string {
  if (n < 1024) return `${n} B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(n < 10 * 1024 ? 1 : 0)} KiB`;
  return `${(n / (1024 * 1024)).toFixed(1)} MiB`;
}

const ELEMENTS: Record<KeyType, [string, string]> = {
  string: ["byte", "bytes"],
  hash: ["field", "fields"],
  list: ["item", "items"],
  set: ["member", "members"],
  zset: ["member", "members"],
  stream: ["entry", "entries"],
  json: ["", ""],
  other: ["", ""],
};

/** The key's size: its length in the type's unit, then its memory when known. */
export function sizeText(info: KeyInfo): string {
  const parts: string[] = [];
  if (info.length !== undefined) {
    const [one, many] = ELEMENTS[info.keyType];
    if (info.keyType === "string") parts.push(bytesText(info.length));
    else if (one)
      parts.push(`${info.length.toLocaleString("en-US")} ${info.length === 1 ? one : many}`);
  }
  if (info.memoryBytes !== undefined) parts.push(`${bytesText(info.memoryBytes)} in memory`);
  return parts.join(" · ");
}

/** The value of row `index` of an Arrow column as JS (bytes, text, number, ...). */
export function cellValue(vector: Vector | null | undefined, index: number): unknown {
  if (!vector?.isValid(index)) return null;
  return vector.get(index) as unknown;
}

/** A `Binary` (or `Utf8`) cell as `BinaryText`. */
export function cellBinary(value: unknown): BinaryText | null {
  if (value instanceof Uint8Array) return binaryText(value);
  if (typeof value === "string") return utf8(value);
  return null;
}

/** One line for showing a `BinaryText` (escaped bytes when not UTF-8). */
export function binaryLabel(value: BinaryText | null): string {
  return value ? displayBinary(value) : "";
}

/** A namespace's scan pattern (`m7:ns:` → `m7:ns:*`), with glob characters escaped. */
export function prefixPattern(prefix: string): string {
  return `${prefix.replace(/[*?[\]\\]/g, (c) => `\\${c}`)}*`;
}

// --- typed element editors -------------------------------------------------------------------

/** Types whose value is a list of elements edited one at a time. */
export type ElementType = "hash" | "list" | "set" | "zset" | "stream";

export function isElementType(type: KeyType): type is ElementType {
  return (
    type === "hash" || type === "list" || type === "set" || type === "zset" || type === "stream"
  );
}

/** An element as read from the value grid (`null` for a new one). */
export interface ElementRow {
  /** list: the index; stream: the entry id. */
  index?: number;
  id?: string;
  field?: BinaryText;
  member?: BinaryText;
  value?: BinaryText;
  score?: number;
  /** stream: the fields as JSON object text. */
  fields?: string;
}

/** What the element form holds (text inputs). */
export interface ElementDraft {
  field: string;
  value: string;
  member: string;
  score: string;
  fields: string;
  /** list: where a new item goes. */
  end: "head" | "tail";
}

export const EMPTY_ELEMENT: ElementDraft = {
  field: "",
  value: "",
  member: "",
  score: "0",
  fields: '{"field": "value"}',
  end: "tail",
};

/** The form for an element read from the grid. */
export function elementDraft(row: ElementRow): ElementDraft {
  return {
    ...EMPTY_ELEMENT,
    field: binaryLabel(row.field ?? null),
    value: binaryLabel(row.value ?? null),
    member: binaryLabel(row.member ?? null),
    score: row.score === undefined ? "0" : String(row.score),
    fields: row.fields ?? EMPTY_ELEMENT.fields,
  };
}

/** The typed text as bytes: the original value when the text was not changed. */
function bytesOf(text: string, original: BinaryText | undefined): BinaryText {
  if (original && binaryLabel(original) === text) return original;
  return utf8(text);
}

function sameBytes(a: BinaryText, b: BinaryText | undefined): boolean {
  if (!b) return false;
  return (
    a.encoding === b.encoding &&
    (a.encoding === "utf8"
      ? a.text === (b as { text: string }).text
      : a.hex === (b as { hex: string }).hex)
  );
}

function score(text: string): number {
  const t = text.trim();
  const n =
    t === "+inf" ? Number.POSITIVE_INFINITY : t === "-inf" ? Number.NEGATIVE_INFINITY : Number(t);
  if (t === "" || Number.isNaN(n)) throw new Error("The score must be a number.");
  return n;
}

function streamFields(text: string): { field: BinaryText; value: BinaryText }[] {
  let parsed: unknown;
  try {
    parsed = JSON.parse(text);
  } catch {
    throw new Error('Enter the entry\'s fields as a JSON object: {"field": "value"}.');
  }
  if (parsed === null || typeof parsed !== "object" || Array.isArray(parsed)) {
    throw new Error('Enter the entry\'s fields as a JSON object: {"field": "value"}.');
  }
  const entries = Object.entries(parsed as Record<string, unknown>);
  if (entries.length === 0) throw new Error("A stream entry needs at least one field.");
  return entries.map(([field, value]) => ({
    field: utf8(field),
    value: utf8(typeof value === "string" ? value : JSON.stringify(value)),
  }));
}

/**
 * The edits that save `draft`: as a new element (`row` null) or over `row`. Throws an `Error`
 * with the user's message when the form is not valid.
 */
export function elementEdits(
  type: ElementType,
  row: ElementRow | null,
  draft: ElementDraft,
): KeyEdit[] {
  switch (type) {
    case "hash": {
      if (draft.field === "") throw new Error("Enter the field name.");
      const field = bytesOf(draft.field, row?.field);
      const value = bytesOf(draft.value, row?.value);
      const edits: KeyEdit[] = [{ type: "hashSet", field, value }];
      // A renamed field: the old one goes.
      if (row?.field && !sameBytes(field, row.field)) {
        edits.push({ type: "hashDelete", fields: [row.field] });
      }
      return edits;
    }
    case "list": {
      const value = bytesOf(draft.value, row?.value);
      if (!row || row.index === undefined) {
        return [{ type: "listPush", end: draft.end, values: [value] }];
      }
      return [
        {
          type: "listSet",
          index: row.index,
          value,
          ...(row.value ? { expected: row.value } : {}),
        },
      ];
    }
    case "set": {
      if (draft.member === "") throw new Error("Enter the member.");
      const member = bytesOf(draft.member, row?.member);
      if (row?.member && sameBytes(member, row.member)) return [];
      const edits: KeyEdit[] = [{ type: "setAdd", members: [member] }];
      if (row?.member) edits.push({ type: "setRemove", members: [row.member] });
      return edits;
    }
    case "zset": {
      if (draft.member === "") throw new Error("Enter the member.");
      const member = bytesOf(draft.member, row?.member);
      const edits: KeyEdit[] = [{ type: "zsetAdd", member, score: score(draft.score) }];
      if (row?.member && !sameBytes(member, row.member)) {
        edits.push({ type: "zsetRemove", members: [row.member] });
      }
      return edits;
    }
    case "stream":
      if (row) throw new Error("Stream entries cannot be changed; add a new entry instead.");
      return [{ type: "streamAdd", fields: streamFields(draft.fields) }];
  }
}

/** The edit that removes `row`. */
export function removeEdit(type: ElementType, row: ElementRow): KeyEdit | null {
  switch (type) {
    case "hash":
      return row.field ? { type: "hashDelete", fields: [row.field] } : null;
    case "list":
      return row.value ? { type: "listRemove", value: row.value, count: 1 } : null;
    case "set":
      return row.member ? { type: "setRemove", members: [row.member] } : null;
    case "zset":
      return row.member ? { type: "zsetRemove", members: [row.member] } : null;
    case "stream":
      return row.id ? { type: "streamDelete", ids: [row.id] } : null;
  }
}

/** The first edit of a new key of `type` (a key exists only with a value). */
export function createEdit(type: Exclude<KeyType, "other">, draft: ElementDraft): KeyEdit {
  switch (type) {
    case "string":
      return { type: "setString", value: utf8(draft.value) };
    case "json": {
      try {
        return { type: "jsonSet", path: "$", value: JSON.parse(draft.value || "{}") };
      } catch {
        throw new Error("The value is not valid JSON.");
      }
    }
    default: {
      const [edit] = elementEdits(type, null, draft);
      if (!edit) throw new Error("Enter the first value.");
      return edit;
    }
  }
}

/** A TTL typed in seconds (blank = no expiry), as `expire.ttlMs`. */
export function ttlInput(text: string): number | undefined {
  const t = text.trim();
  if (t === "") return undefined;
  if (!/^\d+$/.test(t) || Number(t) <= 0)
    throw new Error("Enter the TTL in whole seconds (1 or more).");
  return Number(t) * 1000;
}
