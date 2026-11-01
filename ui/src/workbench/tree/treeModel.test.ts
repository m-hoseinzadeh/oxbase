import { describe, expect, it } from "vitest";
import type { Extras, TreeNode } from "../../api";
import { nodeDetail } from "./treeModel";

function database(name: string, extras: Extras): TreeNode {
  return {
    node: { type: "object", object: { kind: "database", name } },
    key: name,
    label: name,
    expandable: true,
    summary: { ref: { kind: "database", name }, system: false, extras },
  };
}

describe("tree details", () => {
  it("marks databases that are not online (SQL Server state)", () => {
    expect(nodeDetail(database("a", { state: { type: "text", value: "ONLINE" } }))).toBe("");
    expect(nodeDetail(database("b", { state: { type: "text", value: "OFFLINE" } }))).toBe(
      "offline",
    );
    expect(
      nodeDetail(
        database("c", {
          current: { type: "bool", value: true },
          state: { type: "text", value: "RECOVERY_PENDING" },
        }),
      ),
    ).toBe("current · recovery pending");
  });
});
