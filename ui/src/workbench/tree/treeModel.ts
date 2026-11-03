/**
 * The object tree's view model: which nodes are expanded, what their children are (loaded lazily
 * with `tree_children`), and the flat list of rows the panel renders and navigates.
 */
import type { BinaryText, Extras, ObjectRef, TreeNode, TreeNodeRef, Value } from "../../api";
import type { Engine, SessionSummary } from "../../connections/types";

/** A node's id in the panel: its session plus the core's node key. */
export function nodeId(sessionId: string, key: string): string {
  return `${sessionId}::${key}`;
}

export function rootId(sessionId: string): string {
  return nodeId(sessionId, "connection");
}

export type ChildrenState =
  | { status: "loading"; nodes: TreeNode[] }
  | { status: "ok"; nodes: TreeNode[] }
  | { status: "error"; nodes: TreeNode[]; message: string };

export type Row =
  | {
      type: "node";
      id: string;
      sessionId: string;
      level: number;
      label: string;
      /** Muted text after the label (type, row estimate, ...). */
      detail: string;
      expandable: boolean;
      expanded: boolean;
      ref: TreeNodeRef;
      /** Icon name: `connection`, `folder` or the object kind with dashes (`mat-view`). */
      icon: string;
      /** The engine of a connection row (drawn as the engine tile). */
      engine?: Engine;
      system: boolean;
      comment?: string;
      /** The object's listing extras (M7: a Redis key's `key_type`, `ttl_ms`, `key_hex`). */
      extras?: Extras;
      parentId: string | null;
      posInSet: number;
      setSize: number;
    }
  | { type: "loading"; id: string; level: number; parentId: string }
  | { type: "empty"; id: string; level: number; parentId: string }
  | { type: "error"; id: string; level: number; parentId: string; message: string };

function textExtra(value: Value | undefined): string | null {
  if (!value) return null;
  if (value.type === "text") return value.value;
  if (value.type === "int" || value.type === "u_int") return String(value.value);
  return null;
}

function compactCount(n: number): string {
  if (n < 1000) return String(n);
  if (n < 1_000_000) return `${(n / 1000).toFixed(n < 10_000 ? 1 : 0)}k`;
  return `${(n / 1_000_000).toFixed(n < 10_000_000 ? 1 : 0)}M`;
}

/** Muted detail text of an object row. */
export function nodeDetail(node: TreeNode): string {
  const summary = node.summary;
  if (!summary) return "";
  const parts: string[] = [];
  const nativeType = textExtra(summary.extras?.native_type);
  if (nativeType) parts.push(nativeType);
  const pk = summary.extras?.primary_key;
  if (pk?.type === "bool" && pk.value) parts.push("PK");
  const current = summary.extras?.current;
  if (current?.type === "bool" && current.value) parts.push("current");
  // A database that is not online (SQL Server `state`) is listed; opening it fails.
  const state = textExtra(summary.extras?.state);
  if (state && state.toUpperCase() !== "ONLINE") parts.push(state.toLowerCase().replace(/_/g, " "));
  if (summary.row_estimate !== undefined && summary.ref.kind !== "column") {
    parts.push(`~${compactCount(summary.row_estimate)} rows`);
  }
  // M7: Redis keys and namespaces, MongoDB collections.
  const keyType = textExtra(summary.extras?.key_type);
  if (keyType) parts.push(keyType);
  const ttl = summary.extras?.ttl_ms;
  if (ttl && (ttl.type === "int" || ttl.type === "u_int") && ttl.value >= 0) {
    parts.push(`TTL ${ttlLabel(ttl.value)}`);
  }
  const keys = summary.extras?.key_count;
  if (keys && (keys.type === "int" || keys.type === "u_int")) {
    parts.push(`${compactCount(keys.value)} ${keys.value === 1 ? "key" : "keys"}`);
  }
  const docs = summary.extras?.document_count;
  if (
    docs &&
    (docs.type === "int" || docs.type === "u_int") &&
    summary.row_estimate === undefined
  ) {
    parts.push(`${compactCount(docs.value)} docs`);
  }
  const truncated = summary.extras?.truncated;
  if (truncated?.type === "bool" && truncated.value) parts.push("more in the key browser");
  return parts.join(" · ");
}

function ttlLabel(ms: number): string {
  const s = Math.round(ms / 1000);
  if (s < 60) return `${s} s`;
  if (s < 3600) return `${Math.round(s / 60)} min`;
  if (s < 86_400) return `${Math.round(s / 3600)} h`;
  return `${Math.round(s / 86_400)} d`;
}

/**
 * The key of a Redis key node as bytes: its `key_hex` extra when the name is not UTF-8 (the
 * label then shows `\xHH` escapes), else its name.
 */
export function keyBytesOf(object: ObjectRef, extras: Extras | undefined): BinaryText {
  const hex = extras?.key_hex;
  if (hex?.type === "text" && hex.value !== "") return { encoding: "hex", hex: hex.value };
  return { encoding: "utf8", text: object.name };
}

export function iconOf(ref: TreeNodeRef): string {
  switch (ref.type) {
    case "connection":
      return "connection";
    case "folder":
      return "folder";
    case "object":
      return ref.object.kind.replace(/_/g, "-");
  }
}

/** The database a node belongs to (to open a query editor in it). */
export function databaseOf(ref: TreeNodeRef): string | undefined {
  const object: ObjectRef | undefined =
    ref.type === "object" ? ref.object : ref.type === "folder" ? ref.parent : undefined;
  if (!object) return undefined;
  if (object.catalog) return object.catalog;
  return object.kind === "database" && object.name ? object.name : undefined;
}

export interface TreeState {
  expanded: ReadonlySet<string>;
  children: ReadonlyMap<string, ChildrenState>;
}

/** The rows to render: every open session and the expanded nodes below it, in order. */
export function flattenTree(sessions: SessionSummary[], state: TreeState): Row[] {
  const rows: Row[] = [];

  function addChildren(sessionId: string, parentId: string, level: number) {
    const children = state.children.get(parentId);
    if (!children || (children.status === "loading" && children.nodes.length === 0)) {
      rows.push({ type: "loading", id: `${parentId}#loading`, level, parentId });
      return;
    }
    if (children.status === "error") {
      rows.push({
        type: "error",
        id: `${parentId}#error`,
        level,
        parentId,
        message: children.message,
      });
      return;
    }
    if (children.nodes.length === 0) {
      rows.push({ type: "empty", id: `${parentId}#empty`, level, parentId });
      return;
    }
    children.nodes.forEach((node, index) => {
      const id = nodeId(sessionId, node.key);
      const expanded = node.expandable && state.expanded.has(id);
      const comment = node.summary?.comment;
      const extras = node.summary?.extras;
      rows.push({
        type: "node",
        id,
        sessionId,
        level,
        label: node.label,
        detail: nodeDetail(node),
        expandable: node.expandable,
        expanded,
        ref: node.node,
        icon: iconOf(node.node),
        system: node.summary?.system ?? false,
        ...(comment ? { comment } : {}),
        ...(extras ? { extras } : {}),
        parentId,
        posInSet: index + 1,
        setSize: children.nodes.length,
      });
      if (expanded) addChildren(sessionId, id, level + 1);
    });
  }

  sessions.forEach((session, index) => {
    const id = rootId(session.id);
    const expanded = state.expanded.has(id);
    rows.push({
      type: "node",
      id,
      sessionId: session.id,
      level: 1,
      label: session.profile_name,
      detail: session.database ?? "",
      expandable: true,
      expanded,
      ref: { type: "connection" },
      icon: "connection",
      engine: session.engine,
      system: false,
      parentId: null,
      posInSet: index + 1,
      setSize: sessions.length,
    });
    if (expanded) addChildren(session.id, id, 2);
  });
  return rows;
}

/** Ids of every node below `id` whose children are cached (for refresh). */
export function cachedDescendants(
  sessionId: string,
  id: string,
  children: ReadonlyMap<string, ChildrenState>,
): string[] {
  const out: string[] = [];
  const stack = [id];
  while (stack.length > 0) {
    const current = stack.pop() as string;
    for (const node of children.get(current)?.nodes ?? []) {
      const child = nodeId(sessionId, node.key);
      if (children.has(child)) {
        out.push(child);
        stack.push(child);
      }
    }
  }
  return out;
}
