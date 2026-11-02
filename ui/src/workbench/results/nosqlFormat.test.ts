import { Binary, Field, Schema, Table, vectorFromArray } from "apache-arrow";
import { describe, expect, it } from "vitest";
import type { ColumnInfo } from "../../api";
import { bytesText, columnTypeLabel, formatCell, visibleColumns } from "./format";

describe("M7 cell formatting", () => {
  it("hides only the trailing whole-document column", () => {
    const doc: ColumnInfo = {
      name: "$document",
      dataType: "Utf8",
      nullable: false,
      metadata: { "oxbase.role": "document", "ARROW:extension:name": "arrow.json" },
    };
    const id: ColumnInfo = { name: "_id", dataType: "Utf8", nullable: false };
    expect(visibleColumns([id, doc])).toEqual([id]);
    const columns = [id];
    expect(visibleColumns(columns)).toBe(columns);
  });

  it("labels columns with their BSON type", () => {
    expect(
      columnTypeLabel({
        name: "n",
        dataType: "Int64",
        nullable: true,
        metadata: { "oxbase.bson_type": "long" },
      }),
    ).toBe("long");
  });

  it("shows user bytes as text when they are UTF-8, else escaped", () => {
    expect(bytesText(new TextEncoder().encode("héllo"))).toBe("héllo");
    expect(bytesText(Uint8Array.from([0x00, 0xff, 0x22, 0x0a, 0x41]))).toBe('\\x00\\xff\\"\\nA');
    const column: ColumnInfo = {
      name: "value",
      dataType: "Binary",
      nullable: true,
      metadata: { "oxbase.encoding": "binary" },
    };
    const vector = vectorFromArray(
      [Uint8Array.from([0x68, 0x69]), Uint8Array.from([0xfe])],
      new Binary(),
    );
    const table = new Table(new Schema([new Field("value", new Binary(), true)]), {
      value: vector,
    });
    const col = table.getChildAt(0);
    if (!col) throw new Error("no column");
    expect(formatCell(col, 0, column).text).toBe("hi");
    expect(formatCell(col, 1, column).text).toBe("\\xfe");
    // Other binary columns keep the hex form.
    expect(formatCell(col, 0).text).toBe("0x6869");
  });
});
