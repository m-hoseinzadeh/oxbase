/**
 * The filter and sort builder's view model and its translation to the core's `Filter` /
 * `SortKey` (docs/m3-contracts.md §4). The builder edits a tree of groups and conditions; what
 * the user typed goes to the core as `{ type: "text" }` values and the core converts them with
 * the column's type. A raw WHERE (typed SQL) is sent as `rawSql` and checked by the core.
 */
import {
  type Filter,
  type FilterOp,
  filterOpArity,
  type InputKind,
  type SortKey,
  type Value,
} from "../../api";

/** A column the builder offers, with how its values are typed. */
export interface FilterColumn {
  name: string;
  input: InputKind;
}

export interface ConditionNode {
  kind: "condition";
  id: string;
  column: string;
  op: FilterOp;
  /** What the user typed, one entry per value. */
  values: string[];
}

export interface RawNode {
  kind: "raw";
  id: string;
  sql: string;
}

export interface GroupNode {
  kind: "group";
  id: string;
  /** How the items combine. */
  join: "and" | "or";
  /** NOT (...) around the group. */
  negate: boolean;
  items: FilterNode[];
}

export type FilterNode = ConditionNode | RawNode | GroupNode;

/** The whole builder: a tree of conditions, or one raw WHERE text. */
export interface FilterDraft {
  mode: "builder" | "raw";
  root: GroupNode;
  raw: string;
}

let nextId = 0;
export function newNodeId(): string {
  nextId += 1;
  return `f${nextId}`;
}

export function emptyGroup(join: "and" | "or" = "and"): GroupNode {
  return { kind: "group", id: newNodeId(), join, negate: false, items: [] };
}

export function emptyDraft(): FilterDraft {
  return { mode: "builder", root: emptyGroup(), raw: "" };
}

export function newCondition(column = ""): ConditionNode {
  return { kind: "condition", id: newNodeId(), column, op: "eq", values: [""] };
}

export function newRaw(): RawNode {
  return { kind: "raw", id: newNodeId(), sql: "" };
}

export interface OpInfo {
  op: FilterOp;
  label: string;
}

/** Every operator, in menu order, with its label. */
export const FILTER_OPS: readonly OpInfo[] = [
  { op: "eq", label: "=" },
  { op: "ne", label: "≠" },
  { op: "lt", label: "<" },
  { op: "le", label: "≤" },
  { op: "gt", label: ">" },
  { op: "ge", label: "≥" },
  { op: "contains", label: "contains" },
  { op: "notContains", label: "does not contain" },
  { op: "startsWith", label: "starts with" },
  { op: "endsWith", label: "ends with" },
  { op: "like", label: "LIKE" },
  { op: "notLike", label: "NOT LIKE" },
  { op: "in", label: "is one of" },
  { op: "notIn", label: "is not one of" },
  { op: "between", label: "between" },
  { op: "notBetween", label: "not between" },
  { op: "isNull", label: "is NULL" },
  { op: "isNotNull", label: "is not NULL" },
];

const PATTERN_OPS: ReadonlySet<FilterOp> = new Set([
  "contains",
  "notContains",
  "startsWith",
  "endsWith",
  "like",
  "notLike",
]);
const ORDER_OPS: ReadonlySet<FilterOp> = new Set(["lt", "le", "gt", "ge", "between", "notBetween"]);

/** The operators that make sense for a column's input kind. */
export function opsFor(input: InputKind | undefined): OpInfo[] {
  const type = input?.type ?? "text";
  if (type === "boolean") {
    return FILTER_OPS.filter((o) => ["eq", "ne", "isNull", "isNotNull"].includes(o.op));
  }
  if (type === "json" || type === "bytes") {
    return FILTER_OPS.filter((o) => !ORDER_OPS.has(o.op));
  }
  if (type === "enum" || type === "uuid") {
    return FILTER_OPS.filter((o) => !PATTERN_OPS.has(o.op) || o.op === "contains");
  }
  return [...FILTER_OPS];
}

/** How many value inputs an operator shows. */
export function valueSlots(op: FilterOp, current: number): number {
  switch (filterOpArity(op)) {
    case "none":
      return 0;
    case "one":
      return 1;
    case "two":
      return 2;
    case "many":
      return Math.max(1, current);
  }
}

/** A condition with `op`, keeping the values that still fit. */
export function withOp(node: ConditionNode, op: FilterOp): ConditionNode {
  const slots = valueSlots(op, node.values.length);
  const values = node.values.slice(0, slots);
  while (values.length < slots) values.push("");
  return { ...node, op, values };
}

/** A problem that keeps the draft from being sent. */
export interface FilterProblem {
  id: string;
  message: string;
}

function text(value: string): Value {
  return { type: "text", value };
}

function conditionToFilter(
  node: ConditionNode,
  columns: readonly FilterColumn[],
  problems: FilterProblem[],
): Filter | null {
  if (!node.column) {
    problems.push({ id: node.id, message: "Choose a column." });
    return null;
  }
  if (columns.length > 0 && !columns.some((c) => c.name === node.column)) {
    problems.push({ id: node.id, message: `Unknown column ${node.column}.` });
    return null;
  }
  const arity = filterOpArity(node.op);
  if (arity === "none") return { type: "condition", column: node.column, op: node.op };
  let values = node.values;
  // Empty slots of a value list are slots not filled in yet.
  if (arity === "many") values = values.filter((v) => v !== "");
  // An empty text is a real empty string (contract §5): `name = ''` is a valid condition on a
  // text column (and on any column with a pattern operator). Other inputs need a value.
  const input = columns.find((c) => c.name === node.column)?.input.type ?? "text";
  const emptyAllowed = PATTERN_OPS.has(node.op) || input === "text" || input === "other";
  if (values.length === 0 || (!emptyAllowed && values.some((v) => v === ""))) {
    problems.push({
      id: node.id,
      message:
        arity === "two" ? "Enter both values." : "Enter a value (use “is NULL” to match NULLs).",
    });
    return null;
  }
  return { type: "condition", column: node.column, op: node.op, values: values.map(text) };
}

function nodeToFilter(
  node: FilterNode,
  columns: readonly FilterColumn[],
  problems: FilterProblem[],
): Filter | null {
  switch (node.kind) {
    case "condition":
      return conditionToFilter(node, columns, problems);
    case "raw": {
      const sql = node.sql.trim();
      if (!sql) {
        problems.push({ id: node.id, message: "Enter an SQL condition." });
        return null;
      }
      return { type: "rawSql", sql };
    }
    case "group": {
      const items: Filter[] = [];
      for (const item of node.items) {
        const filter = nodeToFilter(item, columns, problems);
        if (filter) items.push(filter);
      }
      // An empty group adds nothing.
      if (items.length === 0) return null;
      const inner: Filter =
        items.length === 1 && !node.negate ? (items[0] as Filter) : { type: node.join, items };
      return node.negate ? { type: "not", item: inner } : inner;
    }
  }
}

/** The core `Filter` of a draft (`undefined` = no filter), or the problems that block it. */
export function draftToFilter(
  draft: FilterDraft,
  columns: readonly FilterColumn[],
): { filter?: Filter; problems: FilterProblem[] } {
  const problems: FilterProblem[] = [];
  if (draft.mode === "raw") {
    const sql = draft.raw.trim();
    return sql ? { filter: { type: "rawSql", sql }, problems } : { problems };
  }
  const filter = nodeToFilter(draft.root, columns, problems);
  return filter ? { filter, problems } : { problems };
}

/** Counts the conditions of a draft (for the toolbar badge). */
export function countConditions(draft: FilterDraft): number {
  if (draft.mode === "raw") return draft.raw.trim() ? 1 : 0;
  const count = (node: FilterNode): number =>
    node.kind === "group" ? node.items.reduce((n, item) => n + count(item), 0) : 1;
  return count(draft.root);
}

/** `root` with the node `id` replaced by `next` (or removed when `next` is null). */
export function replaceNode(root: GroupNode, id: string, next: FilterNode | null): GroupNode {
  const visit = (group: GroupNode): GroupNode => {
    const items: FilterNode[] = [];
    let changed = false;
    for (const item of group.items) {
      if (item.id === id) {
        changed = true;
        if (next) items.push(next);
      } else if (item.kind === "group") {
        const inner = visit(item);
        if (inner !== item) changed = true;
        items.push(inner);
      } else {
        items.push(item);
      }
    }
    return changed ? { ...group, items } : group;
  };
  if (root.id === id) return next?.kind === "group" ? next : root;
  return visit(root);
}

// --- sort ---------------------------------------------------------------------------------

export interface SortDraftKey {
  id: string;
  column: string;
  descending: boolean;
  nulls: "default" | "first" | "last";
}

export function newSortKey(column = ""): SortDraftKey {
  return { id: newNodeId(), column, descending: false, nulls: "default" };
}

/** The core sort keys (keys without a column are skipped). */
export function draftToSort(keys: readonly SortDraftKey[]): SortKey[] {
  return keys
    .filter((k) => k.column)
    .map((k) => ({
      column: k.column,
      descending: k.descending,
      ...(k.nulls === "default" ? {} : { nulls: k.nulls }),
    }));
}

/** Moves item `index` by `delta` places. */
export function move<T>(items: readonly T[], index: number, delta: number): T[] {
  const target = index + delta;
  if (target < 0 || target >= items.length) return [...items];
  const next = [...items];
  const [item] = next.splice(index, 1);
  next.splice(target, 0, item as T);
  return next;
}
