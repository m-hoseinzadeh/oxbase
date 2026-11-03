/**
 * Catalog listings the M8 views need, through the object tree's `tree_children` (the engine's
 * tree layout decides where databases, schemas and tables are; nothing here knows an engine).
 */
import { type ObjectKind, type ObjectRef, type TreeNode, treeChildren } from "../../api";

/** Objects of `kinds` among `nodes`, plus those inside folders of those kinds (one level). */
async function objectsOf(
  sessionId: string,
  nodes: TreeNode[],
  kinds: ReadonlySet<ObjectKind>,
  requestId?: string,
): Promise<ObjectRef[]> {
  const out: ObjectRef[] = [];
  for (const node of nodes) {
    const ref = node.node;
    if (ref.type === "object" && kinds.has(ref.object.kind)) out.push(ref.object);
  }
  for (const node of nodes) {
    const ref = node.node;
    if (ref.type === "folder" && kinds.has(ref.kind)) {
      const children = await treeChildren(sessionId, ref, undefined, requestId);
      for (const child of children) {
        if (child.node.type === "object" && kinds.has(child.node.object.kind)) {
          out.push(child.node.object);
        }
      }
    }
  }
  return out;
}

const DATABASE: ReadonlySet<ObjectKind> = new Set(["database"]);
const SCHEMA: ReadonlySet<ObjectKind> = new Set(["schema"]);
const TABLE: ReadonlySet<ObjectKind> = new Set(["table"]);
const TABLE_OR_VIEW: ReadonlySet<ObjectKind> = new Set(["table", "view"]);

/** The databases of a connection (the tree's root level). */
export async function listDatabases(sessionId: string, requestId?: string): Promise<ObjectRef[]> {
  const nodes = await treeChildren(sessionId, { type: "connection" }, undefined, requestId);
  return objectsOf(sessionId, nodes, DATABASE, requestId);
}

/** The schemas of a database; empty on engines whose databases hold tables directly. */
export async function listSchemas(
  sessionId: string,
  database: ObjectRef,
  requestId?: string,
): Promise<ObjectRef[]> {
  const nodes = await treeChildren(
    sessionId,
    { type: "object", object: database },
    undefined,
    requestId,
  );
  return objectsOf(sessionId, nodes, SCHEMA, requestId);
}

/** The tables (and, with `views`, the views) of a database or schema, sorted by name. */
export async function listTables(
  sessionId: string,
  container: ObjectRef,
  views = false,
  requestId?: string,
): Promise<ObjectRef[]> {
  const kinds = views ? TABLE_OR_VIEW : TABLE;
  const nodes = await treeChildren(
    sessionId,
    { type: "object", object: container },
    undefined,
    requestId,
  );
  const out = await objectsOf(sessionId, nodes, kinds, requestId);
  return out.sort((a, b) =>
    a.kind === b.kind ? a.name.localeCompare(b.name) : a.kind === "table" ? -1 : 1,
  );
}
