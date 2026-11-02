import { describe, expect, it } from "vitest";
import { designerTitle } from "./layout";
import { qualifiedName } from "./names";

describe("table names", () => {
  it("puts the schema before the table name", () => {
    expect(qualifiedName({ kind: "table", catalog: "app", schema: "sales", name: "orders" })).toBe(
      "sales.orders",
    );
  });

  it("uses the catalog where an engine has no schema level", () => {
    expect(qualifiedName({ kind: "table", catalog: "shop", name: "orders" })).toBe("shop.orders");
  });

  it("titles designer tabs with the qualified name", () => {
    expect(
      designerTitle({ sessionId: "s", table: { kind: "table", schema: "m3", name: "m3_edit_pk" } }),
    ).toBe("Design m3.m3_edit_pk");
  });
});
