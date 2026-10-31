import { describe, expect, it } from "vitest";
import {
  draftToFilter,
  draftToSort,
  emptyDraft,
  emptyGroup,
  type FilterColumn,
  type FilterDraft,
  move,
  newCondition,
  newRaw,
  newSortKey,
  opsFor,
  replaceNode,
  withOp,
} from "./filterModel";

const COLUMNS: FilterColumn[] = [
  { name: "id", input: { type: "integer" } },
  { name: "name", input: { type: "text" } },
  { name: "active", input: { type: "boolean" } },
];

function draft(...items: FilterDraft["root"]["items"]): FilterDraft {
  return { ...emptyDraft(), root: { ...emptyGroup(), items } };
}

describe("filter model", () => {
  it("sends no filter for an empty builder or empty groups", () => {
    expect(draftToFilter(emptyDraft(), COLUMNS)).toEqual({ problems: [] });
    expect(draftToFilter(draft(emptyGroup("or")), COLUMNS)).toEqual({ problems: [] });
  });

  it("builds a single condition without a wrapping group, values as text", () => {
    const c = { ...newCondition("id"), op: "gt" as const, values: ["10"] };
    expect(draftToFilter(draft(c), COLUMNS).filter).toEqual({
      type: "condition",
      column: "id",
      op: "gt",
      values: [{ type: "text", value: "10" }],
    });
  });

  it("builds nested AND / OR / NOT groups and raw SQL", () => {
    const inner = {
      ...emptyGroup("or"),
      negate: true,
      items: [
        withOp(newCondition("name"), "isNull"),
        { ...withOp(newCondition("name"), "between"), values: ["a", "m"] },
      ],
    };
    const raw = { ...newRaw(), sql: "  id % 2 = 0 " };
    const inList = { ...withOp(newCondition("id"), "in"), values: ["1", "", "3"] };
    expect(draftToFilter(draft(inner, raw, inList), COLUMNS)).toEqual({
      problems: [],
      filter: {
        type: "and",
        items: [
          {
            type: "not",
            item: {
              type: "or",
              items: [
                { type: "condition", column: "name", op: "isNull" },
                {
                  type: "condition",
                  column: "name",
                  op: "between",
                  values: [
                    { type: "text", value: "a" },
                    { type: "text", value: "m" },
                  ],
                },
              ],
            },
          },
          { type: "rawSql", sql: "id % 2 = 0" },
          {
            type: "condition",
            column: "id",
            op: "in",
            values: [
              { type: "text", value: "1" },
              { type: "text", value: "3" },
            ],
          },
        ],
      },
    });
  });

  it("reports conditions that cannot be sent", () => {
    const noColumn = newCondition("");
    const noValue = newCondition("id");
    const halfBetween = { ...withOp(newCondition("id"), "between"), values: ["1", ""] };
    const unknown = newCondition("ghost");
    const emptyRaw = newRaw();
    const { filter, problems } = draftToFilter(
      draft(noColumn, noValue, halfBetween, unknown, emptyRaw),
      COLUMNS,
    );
    expect(filter).toBeUndefined();
    expect(problems.map((p) => p.id)).toEqual([
      noColumn.id,
      noValue.id,
      halfBetween.id,
      unknown.id,
      emptyRaw.id,
    ]);
    expect(problems[1]?.message).toMatch(/is NULL/);
  });

  it("sends the raw WHERE as one rawSql filter", () => {
    const d: FilterDraft = { ...emptyDraft(), mode: "raw", raw: " a = 'x;y' " };
    expect(draftToFilter(d, COLUMNS).filter).toEqual({ type: "rawSql", sql: "a = 'x;y'" });
    expect(draftToFilter({ ...d, raw: "  " }, COLUMNS).filter).toBeUndefined();
  });

  it("offers operators by column kind and keeps fitting values", () => {
    expect(opsFor({ type: "boolean" }).map((o) => o.op)).toEqual([
      "eq",
      "ne",
      "isNull",
      "isNotNull",
    ]);
    expect(opsFor({ type: "text" }).map((o) => o.op)).toContain("contains");
    const c = { ...newCondition("id"), values: ["5"] };
    expect(withOp(c, "between").values).toEqual(["5", ""]);
    expect(withOp(c, "isNull").values).toEqual([]);
    expect(withOp({ ...c, values: ["1", "2", "3"] }, "in").values).toEqual(["1", "2", "3"]);
  });

  it("replaces and removes nodes anywhere in the tree", () => {
    const leaf = newCondition("id");
    const group = { ...emptyGroup(), items: [leaf] };
    const root = { ...emptyGroup(), items: [group] };
    const renamed = replaceNode(root, leaf.id, { ...leaf, column: "name" });
    expect(renamed.items[0]).toMatchObject({ items: [{ column: "name" }] });
    expect(replaceNode(root, leaf.id, null).items[0]).toMatchObject({ items: [] });
    expect(replaceNode(root, "missing", null)).toBe(root);
  });

  it("builds sort keys and moves them", () => {
    const a = newSortKey("id");
    const b = { ...newSortKey("name"), descending: true, nulls: "last" as const };
    const blank = newSortKey("");
    expect(draftToSort([a, b, blank])).toEqual([
      { column: "id", descending: false },
      { column: "name", descending: true, nulls: "last" },
    ]);
    expect(move([1, 2, 3], 2, -1)).toEqual([1, 3, 2]);
    expect(move([1, 2, 3], 0, -1)).toEqual([1, 2, 3]);
  });
});
