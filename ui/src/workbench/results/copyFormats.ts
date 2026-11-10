/**
 * "Copy as" formats of grid cells: tab-separated text (with or without column names), JSON,
 * Markdown, SQL INSERT statements and an IN list. The cells come as text or `null` (SQL NULL),
 * in the order the grid shows them.
 */
import type { Engine } from "../../api";

export type CellText = string | null;

/** A copied column: its name and whether its values are numbers (unquoted in SQL and JSON). */
export interface CopyColumn {
  name: string;
  numeric: boolean;
}

/** What NULL reads as in text formats (tab-separated, Markdown). */
const NULL_TEXT = "NULL";

/** A tab-separated cell: tabs and line breaks would split it, so such a value is quoted. */
function tsvCell(value: CellText): string {
  if (value === null) return NULL_TEXT;
  if (!/[\t\n\r"]/.test(value)) return value;
  return `"${value.replace(/"/g, '""')}"`;
}

/** Tab-separated lines (what spreadsheets paste), with the column names first when asked. */
export function toTsv(columns: CopyColumn[], rows: CellText[][], withNames: boolean): string {
  const lines = rows.map((row) => row.map(tsvCell).join("\t"));
  if (withNames) lines.unshift(columns.map((c) => tsvCell(c.name)).join("\t"));
  return lines.join("\n");
}

/** `text` with the characters HTML gives a meaning (`&`, `<`, `>`, `"`, `'`) as entities. */
export function escapeHtml(text: string): string {
  return text
    .replaceAll("&", "&amp;")
    .replaceAll("<", "&lt;")
    .replaceAll(">", "&gt;")
    .replaceAll('"', "&quot;")
    .replaceAll("'", "&#39;");
}

/**
 * The HTML table of a plain copy (the clipboard's `text/html`, which spreadsheets and rich
 * editors paste): Glide's layout (each cell's exact value in `gdg-raw-value`, which Glide's own
 * paste reads back; tabs as four spaces and runs of spaces kept for Excel and Google Sheets),
 * but every value escaped, so a cell holding `a</td><td>b` or `<img src=...>` stays text.
 */
export function toHtmlTable(rows: CellText[][]): string {
  const cell = (value: CellText) => {
    const text = value ?? NULL_TEXT;
    const shown = escapeHtml(text)
      .replace(/\t/g, "    ")
      .replace(/ {2,}/g, (spaces) => "<span> </span>".repeat(spaces.length));
    return `<td gdg-raw-value="${escapeHtml(text)}" gdg-format="string">${shown}</td>`;
  };
  const body = rows.map((row) => `<tr>${row.map(cell).join("")}</tr>`).join("");
  return (
    '<style type="text/css"><!--br {mso-data-placement:same-cell;}--></style>' +
    `<table><tbody>${body}</tbody></table>`
  );
}

/** A number as it is, when it is a plain JSON / SQL number; else `null`. */
function numberText(value: string): string | null {
  return /^-?(0|[1-9]\d*)(\.\d+)?([eE][+-]?\d+)?$/.test(value) ? value : null;
}

/** An array of objects, one per row; numbers stay numbers (every digit kept). */
export function toJson(columns: CopyColumn[], rows: CellText[][]): string {
  const objects = rows.map((row) => {
    const fields = columns.map((column, i) => {
      const value = row[i] ?? null;
      const literal =
        value === null
          ? "null"
          : column.numeric && numberText(value) !== null
            ? value
            : JSON.stringify(value);
      return `    ${JSON.stringify(column.name)}: ${literal}`;
    });
    return `  {\n${fields.join(",\n")}\n  }`;
  });
  return objects.length === 0 ? "[]" : `[\n${objects.join(",\n")}\n]`;
}

function markdownCell(value: CellText): string {
  if (value === null) return NULL_TEXT;
  return value.replace(/\\/g, "\\\\").replace(/\|/g, "\\|").replace(/\r?\n/g, "<br>");
}

/** A Markdown table; numeric columns are right-aligned. */
export function toMarkdown(columns: CopyColumn[], rows: CellText[][]): string {
  const head = `| ${columns.map((c) => markdownCell(c.name)).join(" | ")} |`;
  const rule = `| ${columns.map((c) => (c.numeric ? "---:" : "---")).join(" | ")} |`;
  const body = rows.map((row) => `| ${row.map(markdownCell).join(" | ")} |`);
  return [head, rule, ...body].join("\n");
}

/** An identifier quoted for `engine` (`"x"`, `` `x` `` or `[x]`). */
export function quoteIdent(name: string, engine: Engine | null | undefined): string {
  if (engine === "mysql" || engine === "mariadb") return `\`${name.replace(/`/g, "``")}\``;
  if (engine === "mssql") return `[${name.replace(/]/g, "]]")}]`;
  return `"${name.replace(/"/g, '""')}"`;
}

/**
 * A SQL literal: NULL, a number as it is (numeric columns), else a quoted string.
 *
 * MySQL / MariaDB read a backslash in a string as an escape (the default `sql_mode`, which
 * Oxbase's sessions and its MySQL splitter follow), so a backslash is doubled and a NUL is
 * `\0`, as the core's dump literals write them (`BacktickQuoting::quote_str`): otherwise a
 * value ending in `\` or holding `\'` would end the string early and the rest of it would run
 * as SQL.
 */
export function sqlLiteral(
  value: CellText,
  numeric: boolean,
  engine?: Engine | null | undefined,
): string {
  if (value === null) return "NULL";
  if (numeric && numberText(value) !== null) return value;
  const backslashes = engine === "mysql" || engine === "mariadb";
  // Backslashes first, so the backslash of a NUL's `\0` is not doubled.
  const escaped = backslashes
    ? value.replaceAll("\\", "\\\\").replaceAll("'", "''").replaceAll("\0", "\\0")
    : value.replaceAll("'", "''");
  const quoted = `'${escaped}'`;
  // SQL Server reads a plain '...' in the database's code page: N'...' keeps every character.
  return engine === "mssql" ? `N${quoted}` : quoted;
}

/**
 * One INSERT per row into `table` (already qualified and quoted, e.g. `"public"."users"`).
 */
export function toSqlInsert(
  table: string,
  columns: CopyColumn[],
  rows: CellText[][],
  engine: Engine | null | undefined,
): string {
  const names = columns.map((c) => quoteIdent(c.name, engine)).join(", ");
  return rows
    .map((row) => {
      const values = columns.map((c, i) => sqlLiteral(row[i] ?? null, c.numeric, engine));
      return `INSERT INTO ${table} (${names}) VALUES (${values.join(", ")});`;
    })
    .join("\n");
}

/** The distinct values of the cells, in order, as `(1, 2, 3)` or `('a', 'b')`; NULLs dropped. */
export function toInList(
  values: CellText[],
  numeric: boolean,
  engine?: Engine | null | undefined,
): string {
  const seen = new Set<string>();
  const items: string[] = [];
  for (const value of values) {
    if (value === null || seen.has(value)) continue;
    seen.add(value);
    items.push(sqlLiteral(value, numeric, engine));
  }
  return `(${items.join(", ")})`;
}

/**
 * Tab-separated text (a spreadsheet's or our own copy) as rows of cells: `"..."` quotes a cell
 * with tabs, line breaks or quotes (`""` is a quote); a trailing line break ends the last row.
 */
export function parseTsv(text: string): string[][] {
  const rows: string[][] = [];
  let row: string[] = [];
  let cell = "";
  let i = 0;
  const n = text.length;
  let quoted = false;
  let atCellStart = true;
  while (i < n) {
    const c = text[i] as string;
    if (quoted) {
      if (c === '"' && text[i + 1] === '"') {
        cell += '"';
        i += 2;
        continue;
      }
      if (c === '"') {
        quoted = false;
        i += 1;
        continue;
      }
      cell += c;
      i += 1;
      continue;
    }
    if (c === '"' && atCellStart) {
      quoted = true;
      atCellStart = false;
      i += 1;
      continue;
    }
    atCellStart = false;
    if (c === "\t") {
      row.push(cell);
      cell = "";
      atCellStart = true;
    } else if (c === "\n" || c === "\r") {
      if (c === "\r" && text[i + 1] === "\n") i += 1;
      row.push(cell);
      rows.push(row);
      row = [];
      cell = "";
      atCellStart = true;
    } else {
      cell += c;
    }
    i += 1;
  }
  if (!atCellStart || cell !== "" || row.length > 0) {
    row.push(cell);
    rows.push(row);
  }
  return rows;
}
