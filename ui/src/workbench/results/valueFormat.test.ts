import { describe, expect, it } from "vitest";
import { detectFormat, prettyJson, prettyXml } from "./valueFormat";

describe("prettyJson", () => {
  it("re-indents without changing values or key order", () => {
    expect(prettyJson('{"b":1,"a":[1,2,{"x":null}],"e":{},"f":[]}')).toBe(
      [
        "{",
        '  "b": 1,',
        '  "a": [',
        "    1,",
        "    2,",
        "    {",
        '      "x": null',
        "    }",
        "  ],",
        '  "e": {},',
        '  "f": []',
        "}",
      ].join("\n"),
    );
  });

  it("keeps big numbers and strings with escapes and structural characters exactly", () => {
    const text = '{"id":12345678901234567890,"s":"a, \\"b\\": {c} [d]","n":-1.50e+3}';
    const out = prettyJson(text);
    expect(out).toContain('"id": 12345678901234567890');
    expect(out).toContain('"s": "a, \\"b\\": {c} [d]"');
    expect(out).toContain('"n": -1.50e+3');
    expect(JSON.parse(out as string)).toEqual(JSON.parse(text));
  });

  it("refuses what is not JSON", () => {
    expect(prettyJson("{a:1}")).toBeNull();
    expect(prettyJson("hello")).toBeNull();
  });
});

describe("prettyXml", () => {
  it("indents nested tags and keeps text, attributes, comments and CDATA", () => {
    expect(
      prettyXml(
        '<?xml version="1.0"?><root a="x > y"><!-- c --><item id="1">one</item><empty/><data><![CDATA[<raw>]]></data></root>',
      ),
    ).toBe(
      [
        '<?xml version="1.0"?>',
        '<root a="x > y">',
        "  <!-- c -->",
        '  <item id="1">one</item>',
        "  <empty/>",
        "  <data>",
        "    <![CDATA[<raw>]]>",
        "  </data>",
        "</root>",
      ].join("\n"),
    );
  });

  it("refuses unbalanced tags", () => {
    expect(prettyXml("<a><b></a>")).toBeNull();
    expect(prettyXml("<a>")).toBeNull();
  });
});

describe("detectFormat", () => {
  it("recognizes JSON and XML by shape and validity", () => {
    expect(detectFormat(' {"a": 1} ')).toBe("json");
    expect(detectFormat("[1, 2]")).toBe("json");
    expect(detectFormat("{not json}")).toBe("text");
    expect(detectFormat("<a><b/></a>")).toBe("xml");
    expect(detectFormat("<a>")).toBe("text");
    expect(detectFormat("SELECT 1")).toBe("text");
  });
});
