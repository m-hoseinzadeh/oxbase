/**
 * The object tree's view model: which nodes are expanded, what their children are (loaded lazily
 * with `tree_children`), and the flat list of rows the panel renders and navigates.
 */
import type { ObjectRef, TreeNode, TreeNodeRef, Value } from "../../api";
import type { SessionSummary } from "../../connections/types";

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
      /** CSS icon name. */
      icon: string;
      system: boolean;
      comment?: string;
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
  if (summary.row_estimate !== undefined && summary.ref.kind !== "column") {
    parts.push(`~${compactCount(summary.row_estimate)} rows`);
  }
  return parts.join(" · ");
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
      icon: `connection tree-icon--${session.engine}`,
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
