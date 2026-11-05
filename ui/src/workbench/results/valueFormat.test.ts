import { describe, expect, it } from "vitest";
import { detectFormat, imageMime, prettyJson, prettyXml } from "./valueFormat";

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

  it("recognizes HTML by its tags, even when it is not well-formed", () => {
    expect(detectFormat("<!DOCTYPE html><html><body>x</body></html>")).toBe("html");
    expect(detectFormat("<p>one<br>two</p>")).toBe("html");
    expect(detectFormat("<order><item/></order>")).toBe("xml");
  });

  it("recognizes images by their signature", () => {
    const png = new Uint8Array([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0, 0, 0, 13]);
    expect(imageMime(png)).toBe("image/png");
    expect(imageMime(new Uint8Array([0xff, 0xd8, 0xff, 0xe0, 0, 0, 0, 0, 0, 0, 0, 0]))).toBe(
      "image/jpeg",
    );
    const webp = new TextEncoder().encode("RIFF\0\0\0\0WEBPVP8 ");
    expect(imageMime(webp)).toBe("image/webp");
    expect(imageMime(new TextEncoder().encode("hello, world"))).toBeNull();
    expect(imageMime(null)).toBeNull();
  });
});
