import { describe, expect, it } from "vitest";
import type { ObjectRef } from "../../api";
import {
  aggregateRequest,
  EMPTY_FIND,
  findRequest,
  pipelineWrites,
  requestPreview,
  type StageDraft,
} from "./builder";

const DOCS: ObjectRef = { kind: "table", catalog: "oxbase_test", name: "m7_docs" };

function stage(id: number, operator: string, body: string, enabled = true): StageDraft {
  return { id, operator, body, enabled };
}

describe("find builder", () => {
  it("writes a FindRequest with the sort's key order and drops empty parts", () => {
    const { request, problems } = findRequest(DOCS, {
      filter: '{"n": {"$gt": 1}}',
      projection: "",
      sort: '{"b": -1, "a": 1}',
      skip: "0",
      limit: "50",
    });
    expect(problems).toEqual([]);
    expect(request).toEqual({
      collection: DOCS,
      filter: { n: { $gt: 1 } },
      sort: { b: -1, a: 1 },
      limit: 50,
    });
    expect(Object.keys(request?.sort ?? {})).toEqual(["b", "a"]);
    expect(findRequest(DOCS, EMPTY_FIND).request).toEqual({ collection: DOCS });
  });

  it("reports each field that is not valid", () => {
    const { request, problems } = findRequest(DOCS, {
      filter: "{n: 1}",
      projection: "[1]",
      sort: "",
      skip: "-1",
      limit: "ten",
    });
    expect(request).toBeNull();
    expect(problems.map((p) => p.field)).toEqual(["filter", "projection", "skip", "limit"]);
    expect(problems[0]?.message).toMatch(/Extended JSON/);
    expect(problems[1]?.message).toMatch(/object/);
  });
});

describe("aggregate builder", () => {
  it("writes the enabled stages in order", () => {
    const { request, problems } = aggregateRequest(DOCS, [
      stage(1, "$match", '{"g": 7}'),
      stage(2, "$sort", '{"_id": 1}', false),
      stage(3, "$group", '{"_id": null, "total": {"$sum": "$g"}}'),
      stage(4, "$limit", "5"),
    ]);
    expect(problems).toEqual([]);
    expect(request?.pipeline).toEqual([
      { $match: { g: 7 } },
      { $group: { _id: null, total: { $sum: "$g" } } },
      { $limit: 5 },
    ]);
  });

  it("names the stage with a bad operator or body", () => {
    const { request, problems } = aggregateRequest(DOCS, [
      stage(1, "match", "{}"),
      stage(2, "$match", "{oops"),
    ]);
    expect(request).toBeNull();
    expect(problems.map((p) => p.field)).toEqual(["stage:1:operator", "stage:2"]);
  });

  it("knows which pipelines write", () => {
    expect(pipelineWrites([stage(1, "$match", "{}"), stage(2, "$out", '"x"')])).toBe(true);
    expect(pipelineWrites([stage(1, "$merge", "{}", false)])).toBe(false);
  });

  it("previews requests as shell code", () => {
    expect(
      requestPreview(DOCS, {
        type: "find",
        request: { collection: DOCS, filter: { a: 1 }, sort: { b: -1 }, limit: 5 },
      }),
    ).toBe('db.m7_docs.find({"a":1}).sort({"b":-1}).limit(5)');
    expect(
      requestPreview(DOCS, { type: "aggregate", request: { collection: DOCS, pipeline: [] } }),
    ).toBe("db.m7_docs.aggregate([])");
  });
});
