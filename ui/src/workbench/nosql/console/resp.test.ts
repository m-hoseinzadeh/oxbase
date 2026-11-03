import { Field, Float64, Schema, Table, Utf8, vectorFromArray } from "apache-arrow";
import { describe, expect, it } from "vitest";
import type { ColumnInfo } from "../../../api";
import { respLine, respOutput } from "./resp";

function cols(...names: string[]): ColumnInfo[] {
  return names.map((name) => ({ name, dataType: "Utf8", nullable: true }));
}

function textTable(columns: Record<string, (string | null)[]>): Table {
  const fields = Object.keys(columns).map((n) => new Field(n, new Utf8(), true));
  const vectors = Object.fromEntries(
    Object.entries(columns).map(([n, values]) => [n, vectorFromArray(values, new Utf8())]),
  );
  return new Table(new Schema(fields), vectors);
}

describe("RESP rendering", () => {
  it("types single replies like redis-cli", () => {
    expect(respLine("integer", "3")).toMatchObject({ text: "(integer) 3", tone: "integer" });
    expect(respLine("null", null)).toMatchObject({ text: "(nil)", tone: "nil" });
    expect(respLine("string", 'a"b\n')).toMatchObject({ text: '"a\\"b\\n"', tone: "string" });
    expect(respLine("status", "OK")).toMatchObject({ text: "OK", tone: "status" });
    expect(respLine("boolean", "true").text).toBe("(true)");
    expect(respLine("double", "1.5").text).toBe("(double) 1.5");
    expect(respLine("error", "WRONGTYPE x").text).toBe("(error) WRONGTYPE x");
  });

  it("numbers array elements and keys map entries", () => {
    const one = respOutput(
      cols("type", "value", "raw"),
      textTable({ type: ["string"], value: ["v"], raw: [null] }),
      1,
    );
    expect(one).toEqual({ kind: "lines", lines: [{ prefix: "", text: '"v"', tone: "string" }] });

    const array = respOutput(
      cols("#", "type", "value", "raw"),
      textTable({
        "#": ["1", "2"],
        type: ["string", "integer"],
        value: ["a", "7"],
        raw: [null, null],
      }),
      2,
    );
    expect(array.kind === "lines" && array.lines.map((l) => `${l.prefix} ${l.text}`)).toEqual([
      '1) "a"',
      "2) (integer) 7",
    ]);

    const map = respOutput(
      cols("key", "key_raw", "type", "value", "raw"),
      textTable({ key: ["f"], key_raw: [null], type: ["string"], value: ["v"], raw: [null] }),
      1,
    );
    expect(map.kind === "lines" && map.lines[0]).toMatchObject({ prefix: '"f" =>', text: '"v"' });

    const empty = respOutput(
      cols("#", "type", "value", "raw"),
      textTable({ "#": [], type: [], value: [], raw: [] }),
      0,
    );
    expect(empty).toEqual({
      kind: "lines",
      lines: [{ prefix: "", text: "(empty array)", tone: "nil" }],
    });
  });

  it("shows typed shapes as a table", () => {
    const table = new Table(
      new Schema([
        new Field("member", new Utf8(), false),
        new Field("score", new Float64(), false),
      ]),
      {
        member: vectorFromArray(["a", "b"], new Utf8()),
        score: vectorFromArray([1, 2.5], new Float64()),
      },
    );
    const out = respOutput(
      [
        { name: "member", dataType: "Utf8", nullable: false },
        { name: "score", dataType: "Float64", nullable: false },
      ],
      table,
      2,
    );
    expect(out).toEqual({
      kind: "table",
      columns: ["member", "score"],
      rows: [
        ["a", "1"],
        ["b", "2.5"],
      ],
    });
  });
});
