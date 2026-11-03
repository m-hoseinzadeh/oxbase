/**
 * How the Redis console shows a reply (docs/m7-contracts.md §4.3): the driver turns every reply
 * into Arrow rows (generic shapes `type` / `value` / `raw`, `#` for arrays, `key` for maps, and
 * typed shapes for common read commands, see `oxbase_redis::driver`); this turns those rows back
 * into `redis-cli`-like lines, typed by their RESP type: `(integer) 3`, `"text"`, `(nil)`,
 * `1) "a"`. Shapes it does not know are shown as a small table.
 */
import type { Table } from "apache-arrow";
import type { ColumnInfo } from "../../../api";
import { formatCell } from "../../results/format";

export type RespTone =
  | "string"
  | "status"
  | "integer"
  | "double"
  | "bool"
  | "nil"
  | "error"
  | "json"
  | "plain";

export interface RespLine {
  /** `1)`, `2)` for array elements; `key =>` for map entries. */
  prefix: string;
  text: string;
  tone: RespTone;
}

export type RespOutput =
  | { kind: "lines"; lines: RespLine[] }
  | { kind: "table"; columns: string[]; rows: string[][] };

function quoted(text: string): string {
  return `"${text.replace(/\\/g, "\\\\").replace(/"/g, '\\"').replace(/\n/g, "\\n").replace(/\r/g, "\\r")}"`;
}

/** One element as a typed line (`type` / `value` columns of the generic shapes). */
export function respLine(type: string, value: string | null, prefix = ""): RespLine {
  switch (type) {
    case "null":
      return { prefix, text: "(nil)", tone: "nil" };
    case "integer":
      return { prefix, text: `(integer) ${value ?? ""}`, tone: "integer" };
    case "double":
      return { prefix, text: `(double) ${value ?? ""}`, tone: "double" };
    case "bignumber":
      return { prefix, text: `(big number) ${value ?? ""}`, tone: "integer" };
    case "boolean":
      return {
        prefix,
        text: `(${value === "true" || value === "1" ? "true" : "false"})`,
        tone: "bool",
      };
    case "status":
      return { prefix, text: value ?? "", tone: "status" };
    case "error":
      return { prefix, text: `(error) ${value ?? ""}`, tone: "error" };
    case "string":
    case "verbatim":
      return {
        prefix,
        text: value === null ? "(nil)" : quoted(value),
        tone: value === null ? "nil" : "string",
      };
    default:
      // Nested aggregates arrive as JSON text.
      return { prefix, text: value ?? "(nil)", tone: value === null ? "nil" : "json" };
  }
}

function cellText(table: Table, columns: ColumnInfo[], col: number, row: number): string | null {
  const vector = table.getChildAt(col);
  if (!vector) return null;
  const cell = formatCell(vector, row, columns[col]);
  return cell.isNull ? null : cell.text;
}

/** The lines (or table) of one result set. `total` is the result's full row count. */
export function respOutput(columns: ColumnInfo[], table: Table, total: number): RespOutput {
  const names = columns.map((c) => c.name);
  const at = (name: string) => names.indexOf(name);
  const typeCol = at("type");
  const valueCol = at("value");
  const rows = table.numRows;
  const generic =
    typeCol >= 0 &&
    valueCol >= 0 &&
    names.every((n) => ["#", "key", "key_raw", "type", "value", "raw"].includes(n));
  if (generic) {
    const indexCol = at("#");
    const keyCol = at("key");
    if (indexCol < 0 && keyCol < 0) {
      if (rows === 0) return { kind: "lines", lines: [{ prefix: "", text: "(nil)", tone: "nil" }] };
      const type = cellText(table, columns, typeCol, 0) ?? "null";
      return { kind: "lines", lines: [respLine(type, cellText(table, columns, valueCol, 0))] };
    }
    if (total === 0) {
      return {
        kind: "lines",
        lines: [{ prefix: "", text: keyCol >= 0 ? "(empty map)" : "(empty array)", tone: "nil" }],
      };
    }
    const lines: RespLine[] = [];
    for (let r = 0; r < rows; r += 1) {
      const type = cellText(table, columns, typeCol, r) ?? "null";
      const value = cellText(table, columns, valueCol, r);
      if (keyCol >= 0) {
        const key = cellText(table, columns, keyCol, r) ?? "";
        lines.push(respLine(type, value, `${quoted(key)} =>`));
      } else {
        const n =
          indexCol >= 0 ? (cellText(table, columns, indexCol, r) ?? String(r + 1)) : String(r + 1);
        lines.push(respLine(type, value, `${n})`));
      }
    }
    return { kind: "lines", lines };
  }
  const out: string[][] = [];
  for (let r = 0; r < rows; r += 1) {
    out.push(columns.map((_, c) => cellText(table, columns, c, r) ?? "(nil)"));
  }
  return { kind: "table", columns: names, rows: out };
}
