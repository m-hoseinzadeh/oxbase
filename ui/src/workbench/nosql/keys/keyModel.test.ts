import { describe, expect, it } from "vitest";
import { utf8 } from "../../../api";
import {
  createEdit,
  EMPTY_ELEMENT,
  elementDraft,
  elementEdits,
  prefixPattern,
  removeEdit,
  sizeText,
  ttlInput,
  ttlText,
} from "./keyModel";

const BIN = { encoding: "hex", hex: "00fffe0a" } as const;

describe("key model", () => {
  it("formats TTLs and sizes", () => {
    expect(ttlText(undefined)).toBe("no expiry");
    expect(ttlText(500)).toBe("500 ms");
    expect(ttlText(45_000)).toBe("45 s");
    expect(ttlText(3_900_000)).toBe("1 h 5 min");
    expect(ttlText(2 * 86_400_000 + 4 * 3_600_000)).toBe("2 d 4 h");
    const address = { database: "0", key: utf8("k") };
    expect(sizeText({ address, keyType: "hash", length: 3, memoryBytes: 2048 })).toBe(
      "3 fields · 2.0 KiB in memory",
    );
    expect(sizeText({ address, keyType: "string", length: 12 })).toBe("12 B");
    expect(sizeText({ address, keyType: "list", length: 1 })).toBe("1 item");
  });

  it("escapes glob characters in namespace patterns", () => {
    expect(prefixPattern("m7:ns:")).toBe("m7:ns:*");
    expect(prefixPattern("a*[b]?:")).toBe("a\\*\\[b\\]\\?:*");
  });

  it("builds hash edits, renaming a field by set + delete", () => {
    const row = { field: utf8("f1"), value: utf8("v1") };
    expect(elementEdits("hash", row, { ...elementDraft(row), value: "v2" })).toEqual([
      { type: "hashSet", field: utf8("f1"), value: utf8("v2") },
    ]);
    expect(elementEdits("hash", row, { ...elementDraft(row), field: "f2" })).toEqual([
      { type: "hashSet", field: utf8("f2"), value: utf8("v1") },
      { type: "hashDelete", fields: [utf8("f1")] },
    ]);
    expect(() => elementEdits("hash", null, EMPTY_ELEMENT)).toThrow(/field name/);
  });

  it("keeps binary bytes that were not changed", () => {
    const row = { field: utf8("f"), value: BIN };
    const draft = elementDraft(row);
    expect(draft.value).toBe("\\x00\\xff\\xfe\\n");
    expect(elementEdits("hash", row, draft)).toEqual([
      { type: "hashSet", field: utf8("f"), value: BIN },
    ]);
  });

  it("checks list items against the value read", () => {
    const row = { index: 2, value: utf8("old") };
    expect(elementEdits("list", row, { ...elementDraft(row), value: "new" })).toEqual([
      { type: "listSet", index: 2, value: utf8("new"), expected: utf8("old") },
    ]);
    expect(elementEdits("list", null, { ...EMPTY_ELEMENT, value: "x", end: "head" })).toEqual([
      { type: "listPush", end: "head", values: [utf8("x")] },
    ]);
    expect(removeEdit("list", row)).toEqual({ type: "listRemove", value: utf8("old"), count: 1 });
  });

  it("builds set, zset and stream edits", () => {
    const member = { member: utf8("a"), score: 1 };
    expect(elementEdits("zset", member, { ...elementDraft(member), score: "2.5" })).toEqual([
      { type: "zsetAdd", member: utf8("a"), score: 2.5 },
    ]);
    expect(() => elementEdits("zset", member, { ...elementDraft(member), score: "x" })).toThrow(
      /number/,
    );
    expect(elementEdits("set", { member: utf8("a") }, { ...EMPTY_ELEMENT, member: "b" })).toEqual([
      { type: "setAdd", members: [utf8("b")] },
      { type: "setRemove", members: [utf8("a")] },
    ]);
    expect(
      elementEdits("stream", null, { ...EMPTY_ELEMENT, fields: '{"a": "1", "b": 2}' }),
    ).toEqual([
      {
        type: "streamAdd",
        fields: [
          { field: utf8("a"), value: utf8("1") },
          { field: utf8("b"), value: utf8("2") },
        ],
      },
    ]);
    expect(removeEdit("stream", { id: "1-0" })).toEqual({ type: "streamDelete", ids: ["1-0"] });
  });

  it("creates keys and reads TTL input", () => {
    expect(createEdit("string", { ...EMPTY_ELEMENT, value: "hi" })).toEqual({
      type: "setString",
      value: utf8("hi"),
    });
    expect(createEdit("json", { ...EMPTY_ELEMENT, value: '{"a": [1]}' })).toEqual({
      type: "jsonSet",
      path: "$",
      value: { a: [1] },
    });
    expect(() => createEdit("json", { ...EMPTY_ELEMENT, value: "{" })).toThrow(/JSON/);
    expect(ttlInput("")).toBeUndefined();
    expect(ttlInput("60")).toBe(60_000);
    expect(() => ttlInput("0")).toThrow(/seconds/);
  });
});
