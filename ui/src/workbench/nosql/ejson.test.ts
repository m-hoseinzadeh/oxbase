import { describe, expect, it } from "vitest";
import {
  documentId,
  fieldText,
  JsonSyntaxError,
  parseOrdered,
  prettyJson,
  printNode,
  viewOf,
} from "./ejson";

const DOC =
  '{"_id":{"$oid":"65a000000000000000000001"},"b":1,"10":{"$numberLong":"9007199254740993"},"a":[1,"x",{"$numberDecimal":"1.10"}],"d":{"$date":{"$numberLong":"0"}},"n":null}';

describe("ejson", () => {
  it("keeps field order and number text (integer-like keys stay in place)", () => {
    const node = parseOrdered(DOC);
    expect(node.kind).toBe("object");
    if (node.kind !== "object") return;
    expect(node.entries.map((e) => e.key)).toEqual(["_id", "b", "10", "a", "d", "n"]);
    // JSON.parse would move "10" first.
    expect(Object.keys(JSON.parse(DOC))[0]).toBe("10");
    expect(printNode(node, 0)).toBe(DOC);
  });

  it("pretty-prints without reordering and leaves invalid text alone", () => {
    const pretty = prettyJson('{"b":1,"2":[1,2],"e":{}}');
    expect(pretty).toBe('{\n  "b": 1,\n  "2": [\n    1,\n    2\n  ],\n  "e": {}\n}');
    expect(prettyJson("{oops")).toBe("{oops");
  });

  it("extracts the _id as compact text", () => {
    expect(documentId(DOC)).toBe('{"$oid":"65a000000000000000000001"}');
    expect(documentId('{"_id": 7, "x": 1}')).toBe("7");
    expect(documentId('{"x": 1}')).toBeNull();
    expect(fieldText(DOC, "10")).toBe('{"$numberLong":"9007199254740993"}');
  });

  it("reports syntax errors with a position", () => {
    expect(() => parseOrdered('{"a": 1,\n  b: 2}')).toThrow(JsonSyntaxError);
    try {
      parseOrdered('{"a": 1,\n  b: 2}');
    } catch (error) {
      expect((error as Error).message).toMatch(/line 2, column 3/);
    }
    expect(() => parseOrdered("[1, 2] x")).toThrow(/after the value/);
  });

  it("shows Extended JSON wrappers as typed values", () => {
    const view = (text: string) => viewOf(parseOrdered(text));
    expect(view('{"$oid": "65a000000000000000000001"}')).toEqual({
      type: "objectId",
      text: 'ObjectId("65a000000000000000000001")',
    });
    expect(view('{"$numberInt": "5"}')).toEqual({ type: "int", text: "5" });
    expect(view('{"$numberLong": "9007199254740993"}').text).toBe("9007199254740993");
    expect(view('{"$numberDecimal": "1.10"}')).toEqual({ type: "decimal", text: "1.10" });
    expect(view('{"$date": {"$numberLong": "0"}}')).toEqual({
      type: "date",
      text: "1970-01-01T00:00:00.000Z",
    });
    expect(view('{"$binary": {"base64": "AP8=", "subType": "00"}}').text).toBe(
      'BinData(00, "AP8=")',
    );
    expect(view('{"$regularExpression": {"pattern": "^a", "options": "i"}}').text).toBe("/^a/i");
    expect(view('{"a": 1, "b": 2}')).toMatchObject({ type: "object", text: "{ 2 fields }" });
    expect(view("[1]")).toMatchObject({ type: "array", text: "[ 1 item ]" });
    expect(view('"s"')).toEqual({ type: "string", text: "s" });
    expect(view("null")).toEqual({ type: "null", text: "null" });
    // A user object with a $-key and other keys is not a wrapper.
    expect(view('{"$oid": "x", "y": 1}').type).toBe("object");
  });
});
