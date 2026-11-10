import { describe, expect, it } from "vitest";
import {
  type CopyColumn,
  parseTsv,
  quoteIdent,
  sqlLiteral,
  toHtmlTable,
  toInList,
  toJson,
  toMarkdown,
  toSqlInsert,
  toTsv,
} from "./copyFormats";

const COLUMNS: CopyColumn[] = [
  { name: "id", numeric: true },
  { name: "name", numeric: false },
];
const ROWS = [
  ["1", "ann"],
  ["2", null],
  ["12345678901234567890", "it's a\ttab"],
];

describe("copy formats", () => {
  it("copies tab-separated text, quoting what would split a cell", () => {
    expect(toTsv(COLUMNS, ROWS, false)).toBe(
      ["1\tann", "2\tNULL", '12345678901234567890\t"it\'s a\ttab"'].join("\n"),
    );
    expect(toTsv(COLUMNS, ROWS.slice(0, 1), true)).toBe("id\tname\n1\tann");
  });

  it("copies JSON keeping every digit of numbers and NULL as null", () => {
    const json = toJson(COLUMNS, ROWS);
    expect(json).toContain('"id": 12345678901234567890');
    expect(JSON.parse(toJson(COLUMNS, ROWS.slice(0, 2)))).toEqual([
      { id: 1, name: "ann" },
      { id: 2, name: null },
    ]);
    // A numeric column whose text is not a plain number is a string.
    expect(JSON.parse(toJson([{ name: "n", numeric: true }], [["NaN"]]))).toEqual([{ n: "NaN" }]);
    expect(toJson(COLUMNS, [])).toBe("[]");
  });

  it("copies a Markdown table with escaped pipes and right-aligned numbers", () => {
    expect(toMarkdown(COLUMNS, [["1", "a|b\nc"]])).toBe(
      ["| id | name |", "| ---: | --- |", "| 1 | a\\|b<br>c |"].join("\n"),
    );
  });

  it("copies INSERT statements with the engine's quoting", () => {
    expect(toSqlInsert('"public"."t"', COLUMNS, ROWS.slice(1, 3), "postgres")).toBe(
      [
        `INSERT INTO "public"."t" ("id", "name") VALUES (2, NULL);`,
        `INSERT INTO "public"."t" ("id", "name") VALUES (12345678901234567890, 'it''s a\ttab');`,
      ].join("\n"),
    );
    expect(quoteIdent("a`b", "mysql")).toBe("`a``b`");
    expect(quoteIdent("a]b", "mssql")).toBe("[a]]b]");
    expect(sqlLiteral("é", false, "mssql")).toBe("N'é'");
  });

  it("escapes backslashes and NULs in MySQL and MariaDB strings (audit lead 22)", () => {
    // Under the default sql_mode `\'` is an escaped quote: unescaped, this value would end the
    // string and run DROP TABLE.
    const breakout = "x\\'); DROP TABLE t; -- ";
    for (const engine of ["mysql", "mariadb"] as const) {
      expect(toSqlInsert("`d`.`t`", [{ name: "c", numeric: false }], [[breakout]], engine)).toBe(
        "INSERT INTO `d`.`t` (`c`) VALUES ('x\\\\''); DROP TABLE t; -- ');",
      );
      expect(toInList(["a\\", "n\0ul", "o'k"], false, engine)).toBe("('a\\\\', 'n\\0ul', 'o''k')");
    }
    // Standard strings keep a backslash as it is.
    expect(sqlLiteral("a\\b", false, "postgres")).toBe("'a\\b'");
    expect(sqlLiteral("a\\b", false, "mssql")).toBe("N'a\\b'");
    expect(sqlLiteral("a\\b", false, null)).toBe("'a\\b'");
  });

  it("copies an HTML table with every value escaped (audit lead 24)", () => {
    expect(toHtmlTable([["<b>x</b> & 'y'", null], ['a"  b\tc']])).toBe(
      '<style type="text/css"><!--br {mso-data-placement:same-cell;}--></style><table><tbody>' +
        '<tr><td gdg-raw-value="&lt;b&gt;x&lt;/b&gt; &amp; &#39;y&#39;" gdg-format="string">' +
        "&lt;b&gt;x&lt;/b&gt; &amp; &#39;y&#39;</td>" +
        '<td gdg-raw-value="NULL" gdg-format="string">NULL</td></tr>' +
        '<tr><td gdg-raw-value="a&quot;  b\tc" gdg-format="string">' +
        "a&quot;<span> </span><span> </span>b<span> </span><span> </span><span> </span><span> </span>c</td></tr>" +
        "</tbody></table>",
    );
  });

  it("copies an IN list of the distinct values, without NULL", () => {
    expect(toInList(["3", "1", "3", null], true)).toBe("(3, 1)");
    expect(toInList(["a", "o'k"], false)).toBe("('a', 'o''k')");
  });
});

describe("parseTsv", () => {
  it("reads what toTsv writes, quoted cells included", () => {
    const text = toTsv(
      COLUMNS,
      [
        ["1", 'say "hi"\tthere'],
        ["2", "two\nlines"],
      ],
      false,
    );
    expect(parseTsv(text)).toEqual([
      ["1", 'say "hi"\tthere'],
      ["2", "two\nlines"],
    ]);
  });

  it("ends the last row at a trailing line break and reads CRLF", () => {
    expect(parseTsv("a\tb\r\nc\td\r\n")).toEqual([
      ["a", "b"],
      ["c", "d"],
    ]);
    expect(parseTsv("x\t")).toEqual([["x", ""]]);
    expect(parseTsv("")).toEqual([]);
  });
});
