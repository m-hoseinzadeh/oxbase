/**
 * A mocked core for the M8 view tests: three open sessions (two PostgreSQL, one MySQL), their
 * object trees, and a command router tests extend per case.
 */
import { act } from "@testing-library/react";
import type { ObjectRef, TreeNode, TreeNodeRef } from "../api";
import type { SessionSummary } from "../connections/types";
import { tauriCore } from "./tauriMock";

function session(id: string, name: string, engine: SessionSummary["engine"], database?: string) {
  return {
    id,
    profile_id: `p-${id}`,
    profile_name: name,
    engine,
    info: {
      server_version: { major: 1, minor: 0, patch: 0, build: 0, raw: "1.0" },
      tls_in_use: false,
      via_ssh: false,
      latency_ms: 1,
    },
    opened_at: "2026-11-03T10:00:00Z",
    ...(database ? { database } : {}),
  } satisfies SessionSummary;
}

export const PG_SOURCE = session("pg-1", "PG Source", "postgres", "oxbase_test");
export const PG_TARGET = session("pg-2", "PG Target", "postgres", "oxbase_test");
export const MYSQL = session("my-1", "MySQL", "mysql");
export const SESSIONS: SessionSummary[] = [PG_SOURCE, PG_TARGET, MYSQL];

export const PG_DB: ObjectRef = { kind: "database", name: "oxbase_test" };
export const M8_SRC: ObjectRef = { kind: "schema", catalog: "oxbase_test", name: "m8_src" };
export const M8_TGT: ObjectRef = { kind: "schema", catalog: "oxbase_test", name: "m8_tgt" };
export const MY_TARGET: ObjectRef = { kind: "database", name: "m8_xfer_target" };

export function table(
  schema: ObjectRef,
  name: string,
  kind: "table" | "view" = "table",
): ObjectRef {
  return schema.kind === "schema"
    ? { kind, catalog: schema.catalog ?? "", schema: schema.name, name }
    : { kind, catalog: schema.name, name };
}

function object(ref: ObjectRef, expandable = true): TreeNode {
  return {
    node: { type: "object", object: ref },
    key: `object:${ref.kind}:${ref.schema ?? ""}.${ref.name}`,
    label: ref.name,
    expandable,
    summary: { ref, system: false },
  };
}

function folder(parent: ObjectRef, kind: "table" | "view"): TreeNode {
  return {
    node: { type: "folder", parent, kind },
    key: `folder:${kind}:${parent.name}`,
    label: kind === "table" ? "Tables" : "Views",
    expandable: true,
  };
}

/** Tables per container name (schema or database). */
export const TABLES: Record<string, string[]> = {
  m8_src: ["customers", "m8_data", "m8_nokey", "orders"],
  m8_tgt: ["customers", "m8_data", "orders"],
  m8_xfer_target: [],
};

/** The object tree of the mocked servers. */
export function treeOf(sessionId: string, node: TreeNodeRef): TreeNode[] {
  const pg = sessionId !== MYSQL.id;
  if (node.type === "connection") {
    return pg ? [object(PG_DB)] : [object(MY_TARGET)];
  }
  if (node.type === "object" && node.object.kind === "database") {
    return pg ? [object(M8_SRC), object(M8_TGT)] : [folder(node.object, "table")];
  }
  if (node.type === "object" && node.object.kind === "schema") {
    return [folder(node.object, "table"), folder(node.object, "view")];
  }
  if (node.type === "folder" && node.kind === "table") {
    return (TABLES[node.parent.name] ?? []).map((name) => object(table(node.parent, name)));
  }
  if (node.type === "folder" && node.kind === "view" && node.parent.name === "m8_src") {
    return [object(table(node.parent, "active_customers", "view"), false)];
  }
  return [];
}

export type Handler = (args: Record<string, unknown>) => unknown;

/** Every call the mocked core received, in order. */
export const calls: { cmd: string; args: Record<string, unknown> }[] = [];

/** The open channels of run commands, by command name (the last one wins). */
export const channels: Record<string, (event: unknown) => void> = {};

/**
 * Routes commands: the sessions and the tree are built in; `handlers` add or replace commands.
 * A command without a handler fails the test.
 */
export function mockCore(handlers: Record<string, Handler> = {}): void {
  calls.length = 0;
  for (const key of Object.keys(channels)) delete channels[key];
  tauriCore.isTauri.mockReturnValue(true);
  tauriCore.invoke.mockImplementation(async (cmd, raw) => {
    const args = (raw ?? {}) as Record<string, unknown>;
    calls.push({ cmd, args });
    const channel = args.onEvent as { onmessage: (e: unknown) => void } | undefined;
    if (channel && typeof channel === "object" && "onmessage" in channel) {
      channels[cmd] = (event) => act(() => channel.onmessage(event));
    }
    const handler = handlers[cmd];
    if (handler) return handler(args);
    switch (cmd) {
      case "list_open_connections":
        return SESSIONS;
      case "tree_children":
        return treeOf(args.sessionId as string, args.node as TreeNodeRef);
      case "cancel_request":
      case "compare_forget":
      case "file_release":
        return true;
      default:
        throw new Error(`unexpected command ${cmd}`);
    }
  });
}

/** The calls of one command. */
export function callsOf(cmd: string): Record<string, unknown>[] {
  return calls.filter((c) => c.cmd === cmd).map((c) => c.args);
}

/** The `request` argument of a command's call (`index` -1: the last one). */
export function requestOf<T>(cmd: string, index = 0): T {
  const all = callsOf(cmd);
  const found = index < 0 ? all[all.length + index] : all[index];
  if (!found) throw new Error(`no call ${index} of ${cmd}`);
  return found.request as T;
}
