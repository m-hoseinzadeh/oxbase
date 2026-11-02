/**
 * Extended JSON text helpers for the document views and editors (docs/m7-contracts.md §5.1).
 *
 * Documents cross the IPC as Extended JSON **text** and must go back exactly as they came:
 * `JSON.parse` would move integer-like keys (`"10"`) before the others and turn
 * `{"$numberLong": "9007199254740993"}`-sized numbers into doubles. So this module parses into an
 * ordered tree that keeps every scalar's source text, and prints from it.
 */

export type JsonNode =
  | { kind: "object"; entries: { key: string; value: JsonNode }[]; raw: string }
  | { kind: "array"; items: JsonNode[]; raw: string }
  | { kind: "string"; value: string; raw: string }
  | { kind: "number"; raw: string }
  | { kind: "bool"; value: boolean; raw: string }
  | { kind: "null"; raw: string };

export class JsonSyntaxError extends Error {
  constructor(
    message: string,
    readonly offset: number,
  ) {
    super(message);
    this.name = "JsonSyntaxError";
  }
}

/** Line and column (1-based) of `offset` in `text`. */
export function lineColumn(text: string, offset: number): { line: number; column: number } {
  const before = text.slice(0, offset);
  const line = before.split("\n").length;
  return { line, column: offset - (before.lastIndexOf("\n") + 1) + 1 };
}

/** Parses JSON text into an ordered tree; throws {@link JsonSyntaxError} with the position. */
export function parseOrdered(text: string): JsonNode {
  let i = 0;
  const fail = (what: string): never => {
    const { line, column } = lineColumn(text, i);
    throw new JsonSyntaxError(`${what} at line ${line}, column ${column}`, i);
  };
  const ws = () => {
    while (i < text.length && " \t\n\r".includes(text[i] as string)) i += 1;
  };
  const stringAt = (): { value: string; raw: string } => {
    const start = i;
    i += 1;
    let value = "";
    for (;;) {
      if (i >= text.length) fail("Unterminated string");
      const c = text[i] as string;
      if (c === '"') {
        i += 1;
        return { value, raw: text.slice(start, i) };
      }
      if (c === "\\") {
        const e = text[i + 1];
        const simple: Record<string, string> = {
          '"': '"',
          "\\": "\\",
          "/": "/",
          b: "\b",
          f: "\f",
          n: "\n",
          r: "\r",
          t: "\t",
        };
        if (e !== undefined && e in simple) {
          value += simple[e];
          i += 2;
        } else if (e === "u" && /^[0-9a-fA-F]{4}$/.test(text.slice(i + 2, i + 6))) {
          value += String.fromCharCode(Number.parseInt(text.slice(i + 2, i + 6), 16));
          i += 6;
        } else {
          fail("Bad escape in string");
        }
      } else if (c < " ") {
        fail("Control character in string");
      } else {
        value += c;
        i += 1;
      }
    }
  };
  const value = (): JsonNode => {
    ws();
    const start = i;
    const c = text[i];
    if (c === "{") {
      i += 1;
      const entries: { key: string; value: JsonNode }[] = [];
      ws();
      if (text[i] === "}") {
        i += 1;
        return { kind: "object", entries, raw: text.slice(start, i) };
      }
      for (;;) {
        ws();
        if (text[i] !== '"') fail('Expected a quoted key (Extended JSON needs "key")');
        const key = stringAt().value;
        ws();
        if (text[i] !== ":") fail("Expected ':'");
        i += 1;
        entries.push({ key, value: value() });
        ws();
        if (text[i] === ",") {
          i += 1;
          continue;
        }
        if (text[i] === "}") {
          i += 1;
          return { kind: "object", entries, raw: text.slice(start, i) };
        }
        fail("Expected ',' or '}'");
      }
    }
    if (c === "[") {
      i += 1;
      const items: JsonNode[] = [];
      ws();
      if (text[i] === "]") {
        i += 1;
        return { kind: "array", items, raw: text.slice(start, i) };
      }
      for (;;) {
        items.push(value());
        ws();
        if (text[i] === ",") {
          i += 1;
          continue;
        }
        if (text[i] === "]") {
          i += 1;
          return { kind: "array", items, raw: text.slice(start, i) };
        }
        fail("Expected ',' or ']'");
      }
    }
    if (c === '"') {
      const s = stringAt();
      return { kind: "string", value: s.value, raw: s.raw };
    }
    const number = /^-?(0|[1-9]\d*)(\.\d+)?([eE][+-]?\d+)?/.exec(text.slice(i));
    if (number) {
      i += number[0].length;
      return { kind: "number", raw: number[0] };
    }
    for (const [word, node] of [
      ["true", { kind: "bool", value: true, raw: "true" }],
      ["false", { kind: "bool", value: false, raw: "false" }],
      ["null", { kind: "null", raw: "null" }],
    ] as const) {
      if (text.startsWith(word, i)) {
        i += word.length;
        return node;
      }
    }
    return fail(i >= text.length ? "Unexpected end of text" : "Unexpected character");
  };
  const root = value();
  ws();
  if (i < text.length) fail("Unexpected text after the value");
  return root;
}

/** Prints a node: scalars exactly as written, containers indented (or compact). */
export function printNode(node: JsonNode, indent = 2, level = 0): string {
  const pad = (n: number) => (indent > 0 ? `\n${" ".repeat(indent * n)}` : "");
  const sep = indent > 0 ? ": " : ":";
  switch (node.kind) {
    case "object":
      if (node.entries.length === 0) return "{}";
      return `{${node.entries
        .map(
          (e) =>
            `${pad(level + 1)}${JSON.stringify(e.key)}${sep}${printNode(e.value, indent, level + 1)}`,
        )
        .join(",")}${pad(level)}}`;
    case "array":
      if (node.items.length === 0) return "[]";
      return `[${node.items
        .map((item) => `${pad(level + 1)}${printNode(item, indent, level + 1)}`)
        .join(",")}${pad(level)}]`;
    default:
      return node.raw;
  }
}

/** `text` re-indented without reordering anything; invalid text comes back unchanged. */
export function prettyJson(text: string, indent = 2): string {
  try {
    return printNode(parseOrdered(text), indent);
  } catch {
    return text;
  }
}

/** The compact text of the field `key` of a document's top level, or `null`. */
export function fieldText(document: string, key: string): string | null {
  const root = parseOrdered(document);
  if (root.kind !== "object") return null;
  const entry = root.entries.find((e) => e.key === key);
  return entry ? printNode(entry.value, 0) : null;
}

/** The document's `_id` as compact Extended JSON text (what `DocumentEdit.id` takes). */
export function documentId(document: string): string | null {
  return fieldText(document, "_id");
}

/** How the views show one Extended JSON value. */
export interface ValueView {
  /** BSON type name (`objectId`, `int`, `string`, `object`, ...). */
  type: string;
  /** One-line text (shell-like for wrappers: `ObjectId("…")`). */
  text: string;
  /** Child entries of a plain object or array (wrappers are leaves). */
  children?: { key: string; value: JsonNode }[];
}

function scalarText(node: JsonNode | undefined): string {
  if (!node) return "";
  if (node.kind === "string") return node.value;
  return node.kind === "object" || node.kind === "array" ? printNode(node, 0) : node.raw;
}

function member(node: JsonNode, key: string): JsonNode | undefined {
  return node.kind === "object" ? node.entries.find((e) => e.key === key)?.value : undefined;
}

/** A `$date` value as ISO text (canonical `{"$numberLong": ms}` or relaxed ISO string). */
function dateText(value: JsonNode): string {
  if (value.kind === "string") return value.value;
  const ms = Number(scalarText(member(value, "$numberLong") ?? value));
  if (!Number.isFinite(ms)) return printNode(value, 0);
  const d = new Date(ms);
  return Number.isNaN(d.getTime()) ? String(ms) : d.toISOString();
}

/** Extended JSON wrapper objects (one `$` key) shown as one typed value. */
function wrapper(node: JsonNode): ValueView | null {
  if (node.kind !== "object" || node.entries.length === 0) return null;
  const first = node.entries[0];
  if (!first?.key.startsWith("$")) return null;
  const keys = node.entries.map((e) => e.key);
  const v = first.value;
  switch (first.key) {
    case "$oid":
      return keys.length === 1 ? { type: "objectId", text: `ObjectId("${scalarText(v)}")` } : null;
    case "$numberInt":
      return keys.length === 1 ? { type: "int", text: scalarText(v) } : null;
    case "$numberLong":
      return keys.length === 1 ? { type: "long", text: scalarText(v) } : null;
    case "$numberDouble":
      return keys.length === 1 ? { type: "double", text: scalarText(v) } : null;
    case "$numberDecimal":
      return keys.length === 1 ? { type: "decimal", text: scalarText(v) } : null;
    case "$date":
      return keys.length === 1 ? { type: "date", text: dateText(v) } : null;
    case "$binary": {
      if (keys.length !== 1) return null;
      const base64 = scalarText(member(v, "base64"));
      const subType = scalarText(member(v, "subType"));
      return { type: "binData", text: `BinData(${subType || "00"}, "${base64}")` };
    }
    case "$uuid":
      return keys.length === 1 ? { type: "binData", text: `UUID("${scalarText(v)}")` } : null;
    case "$timestamp":
      return keys.length === 1
        ? {
            type: "timestamp",
            text: `Timestamp(${scalarText(member(v, "t"))}, ${scalarText(member(v, "i"))})`,
          }
        : null;
    case "$regularExpression":
      return keys.length === 1
        ? {
            type: "regex",
            text: `/${scalarText(member(v, "pattern"))}/${scalarText(member(v, "options"))}`,
          }
        : null;
    case "$minKey":
      return keys.length === 1 ? { type: "minKey", text: "MinKey" } : null;
    case "$maxKey":
      return keys.length === 1 ? { type: "maxKey", text: "MaxKey" } : null;
    case "$undefined":
      return keys.length === 1 ? { type: "undefined", text: "undefined" } : null;
    case "$symbol":
      return keys.length === 1 ? { type: "symbol", text: scalarText(v) } : null;
    case "$code":
      return keys.every((k) => k === "$code" || k === "$scope")
        ? { type: "javascript", text: scalarText(v) }
        : null;
    case "$dbPointer":
      return keys.length === 1 ? { type: "dbPointer", text: printNode(v, 0) } : null;
    default:
      return null;
  }
}

/** Type and display text of a value; plain objects and arrays list their children. */
export function viewOf(node: JsonNode): ValueView {
  const wrapped = wrapper(node);
  if (wrapped) return wrapped;
  switch (node.kind) {
    case "object":
      return {
        type: "object",
        text: `{ ${node.entries.length} ${node.entries.length === 1 ? "field" : "fields"} }`,
        children: node.entries,
      };
    case "array":
      return {
        type: "array",
        text: `[ ${node.items.length} ${node.items.length === 1 ? "item" : "items"} ]`,
        children: node.items.map((value, index) => ({ key: String(index), value })),
      };
    case "string":
      return { type: "string", text: node.value };
    case "number":
      return { type: /[.eE]/.test(node.raw) ? "double" : "int", text: node.raw };
    case "bool":
      return { type: "bool", text: node.raw };
    case "null":
      return { type: "null", text: "null" };
  }
}
