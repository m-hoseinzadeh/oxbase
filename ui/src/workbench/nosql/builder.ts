/**
 * The find / aggregate builder's model (docs/m7-contracts.md §4.2): the drafts the collection
 * tab edits as text, and their conversion to `FindRequest` / `AggregateRequest` with a problem
 * per field that is not valid Extended JSON.
 */
import type { AggregateRequest, FindRequest, Json, ObjectRef } from "../../api";
import { JsonSyntaxError } from "./ejson";

export interface FindDraft {
  filter: string;
  projection: string;
  /** Key order is the sort order. */
  sort: string;
  skip: string;
  limit: string;
}

export const EMPTY_FIND: FindDraft = { filter: "", projection: "", sort: "", skip: "", limit: "" };

export interface StageDraft {
  /** Stable id for React keys and focus. */
  id: number;
  /** `$match`, `$group`, ... */
  operator: string;
  /** The stage's value as Extended JSON text (`{...}`, a number for `$limit`, ...). */
  body: string;
  /** Disabled stages are kept in the list and left out of the pipeline. */
  enabled: boolean;
}

/** Stage operators offered in the list (any other `$name` can be typed). */
export const STAGE_OPERATORS = [
  "$match",
  "$project",
  "$addFields",
  "$set",
  "$unset",
  "$group",
  "$sort",
  "$limit",
  "$skip",
  "$unwind",
  "$lookup",
  "$count",
  "$facet",
  "$bucket",
  "$sample",
  "$replaceRoot",
  "$sortByCount",
  "$out",
  "$merge",
] as const;

/** A starting body per operator, so a new stage is valid at once. */
export const STAGE_TEMPLATES: Record<string, string> = {
  $match: "{}",
  $project: '{"_id": 1}',
  $addFields: "{}",
  $set: "{}",
  $unset: '"field"',
  $group: '{"_id": null, "count": {"$sum": 1}}',
  $sort: '{"_id": 1}',
  $limit: "10",
  $skip: "0",
  $unwind: '"$field"',
  $lookup: '{"from": "", "localField": "", "foreignField": "", "as": ""}',
  $count: '"count"',
  $facet: "{}",
  $bucket: '{"groupBy": "$field", "boundaries": [0, 10], "default": "other"}',
  $sample: '{"size": 10}',
  $replaceRoot: '{"newRoot": "$field"}',
  $sortByCount: '"$field"',
  $out: '"collection"',
  $merge: '{"into": "collection"}',
};

/** Stages that write to a collection: the run asks first. */
export const WRITING_STAGES: ReadonlySet<string> = new Set(["$out", "$merge"]);

export type FindField = keyof FindDraft;

export interface BuilderProblem {
  /** `filter`, `projection`, ... or `stage:<id>` (`stage:<id>:operator` for a bad name). */
  field: string;
  message: string;
}

function jsonValue(text: string): Json {
  try {
    return JSON.parse(text) as Json;
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error);
    throw new JsonSyntaxError(`Not valid Extended JSON: ${message}`, 0);
  }
}

function objectField(
  draft: FindDraft,
  field: "filter" | "projection" | "sort",
  problems: BuilderProblem[],
): { [key: string]: Json } | undefined {
  const text = draft[field].trim();
  if (text === "") return undefined;
  try {
    const value = jsonValue(text);
    if (value === null || typeof value !== "object" || Array.isArray(value)) {
      problems.push({ field, message: 'Enter an object, like {"name": 1}.' });
      return undefined;
    }
    return value;
  } catch (error) {
    problems.push({ field, message: (error as Error).message });
    return undefined;
  }
}

function countField(
  draft: FindDraft,
  field: "skip" | "limit",
  problems: BuilderProblem[],
): number | undefined {
  const text = draft[field].trim();
  if (text === "") return undefined;
  if (!/^\d+$/.test(text) || !Number.isSafeInteger(Number(text))) {
    problems.push({ field, message: "Enter a whole number of documents (0 or more)." });
    return undefined;
  }
  return Number(text);
}

/** The find request of `draft`, or the problems that stop it. */
export function findRequest(
  collection: ObjectRef,
  draft: FindDraft,
): { request: FindRequest | null; problems: BuilderProblem[] } {
  const problems: BuilderProblem[] = [];
  const filter = objectField(draft, "filter", problems);
  const projection = objectField(draft, "projection", problems);
  const sort = objectField(draft, "sort", problems);
  const skip = countField(draft, "skip", problems);
  const limit = countField(draft, "limit", problems);
  if (problems.length > 0) return { request: null, problems };
  const request: FindRequest = { collection };
  if (filter && Object.keys(filter).length > 0) request.filter = filter;
  if (projection && Object.keys(projection).length > 0) request.projection = projection;
  if (sort && Object.keys(sort).length > 0) request.sort = sort;
  if (skip !== undefined && skip > 0) request.skip = skip;
  if (limit !== undefined && limit > 0) request.limit = limit;
  return { request, problems };
}

/** The aggregate request of the enabled stages, or the problems that stop it. */
export function aggregateRequest(
  collection: ObjectRef,
  stages: StageDraft[],
): { request: AggregateRequest | null; problems: BuilderProblem[] } {
  const problems: BuilderProblem[] = [];
  const pipeline: { [key: string]: Json }[] = [];
  for (const stage of stages) {
    if (!stage.enabled) continue;
    const operator = stage.operator.trim();
    if (!/^\$[A-Za-z][A-Za-z0-9]*$/.test(operator)) {
      problems.push({
        field: `stage:${stage.id}:operator`,
        message: "A stage name starts with $ (like $match).",
      });
      continue;
    }
    try {
      pipeline.push({ [operator]: jsonValue(stage.body.trim() || "{}") });
    } catch (error) {
      problems.push({ field: `stage:${stage.id}`, message: (error as Error).message });
    }
  }
  if (problems.length > 0) return { request: null, problems };
  return { request: { collection, pipeline }, problems };
}

/** Whether the enabled stages write to a collection (`$out` / `$merge`). */
export function pipelineWrites(stages: StageDraft[]): boolean {
  return stages.some((s) => s.enabled && WRITING_STAGES.has(s.operator.trim()));
}

/** The request as mongo-shell code (for the status line and copying). */
export function requestPreview(
  collection: ObjectRef,
  query: { type: "find"; request: FindRequest } | { type: "aggregate"; request: AggregateRequest },
): string {
  const name = collection.name;
  if (query.type === "aggregate") {
    return `db.${name}.aggregate(${JSON.stringify(query.request.pipeline)})`;
  }
  const r = query.request;
  let text = `db.${name}.find(${JSON.stringify(r.filter ?? {})}${r.projection ? `, ${JSON.stringify(r.projection)}` : ""})`;
  if (r.sort) text += `.sort(${JSON.stringify(r.sort)})`;
  if (r.skip) text += `.skip(${r.skip})`;
  if (r.limit) text += `.limit(${r.limit})`;
  return text;
}
