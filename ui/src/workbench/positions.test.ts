import { describe, expect, it } from "vitest";
import { offsetAt, spanText, sqlPreview } from "./positions";

describe("positions", () => {
  it("maps 1-based lines and UTF-16 columns to offsets", () => {
    const text = "SELECT 'é😀';\nSELECT 2;\n";
    expect(offsetAt(text, 1, 1)).toBe(0);
    expect(offsetAt(text, 2, 1)).toBe(text.indexOf("SELECT 2"));
    expect(offsetAt(text, 9, 1)).toBe(text.length);
    const span = {
      index: 0,
      startByte: 0,
      endByte: 0,
      startLine: 1,
      startColumn: 1,
      endLine: 1,
      // "SELECT 'é😀'" is 12 UTF-16 units (the emoji counts 2).
      endColumn: 13,
      returnsRows: true,
    };
    expect(spanText(text, span)).toBe("SELECT 'é😀'");
  });

  it("previews SQL on one line", () => {
    expect(sqlPreview("SELECT\n  1,\n  2")).toBe("SELECT 1, 2");
    expect(sqlPreview("x".repeat(10), 5)).toBe("xxxx…");
  });
});
