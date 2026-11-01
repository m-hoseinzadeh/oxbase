import { describe, expect, it } from "vitest";
import type { CompletionItem } from "../../../api";
import { ENUMS } from "../../../test/fakeMonaco";
import { TextPositions, toSuggestions } from "./mapping";

function item(label: string, start: number, end: number, filterText: string | null = null) {
  return {
    label,
    kind: "table",
    insertText: label,
    isSnippet: false,
    detail: null,
    documentation: null,
    sortText: `1-${label}`,
    filterText,
    range: { start, end },
  } satisfies CompletionItem;
}

/**
 * The text Monaco scores an item against: from the item's range start to the cursor (see
 * Monaco's `CompletionModel`). The tests check it is a prefix-compatible match of the filter.
 */
function typedFor(text: string, line: number, startColumn: number, cursorColumn: number) {
  const lines = text.split("\n");
  return (lines[line - 1] ?? "").slice(startColumn - 1, cursorColumn - 1);
}

describe("quoted identifiers", () => {
  it.each([
    ['SELECT * FROM "cu', '"'],
    ["SELECT * FROM `cu", "`"],
    ["SELECT * FROM [cu", "["],
  ])("puts the typed quote in front of the filter text (%s)", (text, quote) => {
    const cursor = text.length;
    // The core's word includes the opening quote (docs/m6-contracts.md §4.2).
    const [suggestion] = toSuggestions([item("customers", 14, cursor)], text, cursor, ENUMS);
    expect(suggestion?.filterText).toBe(`${quote}customers`);
    const range = suggestion?.range.insert;
    const typed = typedFor(text, 1, range?.startColumn ?? 0, range?.endColumn ?? 0);
    expect(typed).toBe(`${quote}cu`);
    expect(suggestion?.filterText?.toLowerCase().startsWith(typed.toLowerCase())).toBe(true);
  });

  it("keeps the core's filter text and quotes it when the word starts with a quote", () => {
    const text = 'SELECT * FROM "O';
    const [quoted] = toSuggestions([item('"Orders"', 14, 16, "Orders")], text, 16, ENUMS);
    expect(quoted?.filterText).toBe('"Orders');
    // A label that already starts with the quote needs nothing.
    const [own] = toSuggestions([item('"Orders"', 14, 16)], text, 16, ENUMS);
    expect(own?.filterText).toBeUndefined();
    // A bare trigger: the word is just the quote.
    const bare = 'SELECT * FROM "';
    const [onlyQuote] = toSuggestions([item("customers", 14, 15)], bare, 15, ENUMS);
    expect(onlyQuote?.filterText).toBe('"customers');
  });

  it("leaves unquoted words alone", () => {
    const text = "SELECT * FROM cu";
    const [plain, own] = toSuggestions(
      [item("customers", 14, 16), item("Customers", 14, 16, "customers")],
      text,
      16,
      ENUMS,
    );
    expect(plain?.filterText).toBeUndefined();
    expect(own?.filterText).toBe("customers");
  });
});

describe("TextPositions", () => {
  it("counts lines like Monaco (\\r\\n, \\r and \\n) and clamps offsets", () => {
    const p = new TextPositions("ab\r\ncd\ref\ng");
    expect(p.positionAt(0)).toEqual({ lineNumber: 1, column: 1 });
    expect(p.positionAt(2)).toEqual({ lineNumber: 1, column: 3 });
    expect(p.positionAt(4)).toEqual({ lineNumber: 2, column: 1 });
    expect(p.positionAt(7)).toEqual({ lineNumber: 3, column: 1 });
    expect(p.positionAt(10)).toEqual({ lineNumber: 4, column: 1 });
    expect(p.positionAt(99)).toEqual({ lineNumber: 4, column: 2 });
    expect(p.positionAt(-3)).toEqual({ lineNumber: 1, column: 1 });
  });
});
