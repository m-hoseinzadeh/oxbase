import { act, fireEvent, render, screen, within } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { ObjectRef, TreeNode, TreeNodeRef } from "../../api";
import { refreshSessions, resetConnectionsStore } from "../../connections/store";
import type { SessionSummary } from "../../connections/types";
import { tauriCore } from "../../test/tauriMock";
import { setDockviewApi } from "../layout";
import { ObjectTreePanel } from "./ObjectTreePanel";

vi.mock("@tauri-apps/api/core", async () => (await import("../../test/tauriMock")).tauriCore);

function session(id: string, name: string, engine: "mongodb" | "redis"): SessionSummary {
  return {
    id,
    profile_id: `p-${id}`,
    profile_name: name,
    engine,
    info: {
      server_version: { major: 7, minor: 0, patch: 45, build: 0, raw: "7.0.45" },
      tls_in_use: false,
      via_ssh: false,
      latency_ms: 1,
    },
    opened_at: "2026-11-02T10:00:00Z",
  };
}

const DB: ObjectRef = { kind: "database", name: "oxbase_test" };
const DOCS: ObjectRef = { kind: "table", catalog: "oxbase_test", name: "m7_docs" };
const RDB: ObjectRef = { kind: "database", name: "0" };
const NS: ObjectRef = { kind: "schema", catalog: "0", name: "m7:" };
const HASH: ObjectRef = { kind: "key", catalog: "0", schema: "m7:", name: "m7:hash" };
const BIN: ObjectRef = { kind: "key", catalog: "0", schema: "m7:", name: "m7:\\xff" };

function node(object: ObjectRef, expandable: boolean, extras?: TreeNode["summary"]): TreeNode {
  return {
    node: { type: "object", object },
    key: `${object.kind}:${object.name}`,
    label: object.name,
    expandable,
    summary: extras ?? { ref: object, system: false },
  };
}

function childrenOf(sessionId: string, ref: TreeNodeRef): TreeNode[] {
  if (sessionId === "mongo") {
    if (ref.type === "connection") return [node(DB, true)];
    if (ref.type === "object" && ref.object.kind === "database") {
      return [
        {
          node: { type: "folder", parent: DB, kind: "table" },
          key: "f",
          label: "Collections",
          expandable: true,
        },
      ];
    }
    if (ref.type === "folder") return [node(DOCS, true)];
    return [];
  }
  if (ref.type === "connection") return [node(RDB, true)];
  if (ref.type === "object" && ref.object.kind === "database") return [node(NS, true)];
  if (ref.type === "object" && ref.object.kind === "schema") {
    return [
      node(HASH, false, {
        ref: HASH,
        system: false,
        extras: {
          key_type: { type: "text", value: "hash" },
          ttl_ms: { type: "int", value: 90_000 },
        },
      }),
      node(BIN, false, {
        ref: BIN,
        system: false,
        extras: {
          key_type: { type: "text", value: "string" },
          key_hex: { type: "text", value: "6d373aff" },
        },
      }),
    ];
  }
  return [];
}

describe("ObjectTreePanel for MongoDB and Redis", () => {
  const added: Array<{ id: string; component: string; params: Record<string, unknown> }> = [];
  beforeEach(() => {
    resetConnectionsStore();
    added.length = 0;
    tauriCore.invoke.mockReset();
    tauriCore.isTauri.mockReturnValue(true);
    tauriCore.invoke.mockImplementation(async (cmd, args) => {
      const a = (args ?? {}) as Record<string, unknown>;
      if (cmd === "list_open_connections") {
        return [session("mongo", "Mongo", "mongodb"), session("redis", "Cache", "redis")];
      }
      if (cmd === "tree_children") return childrenOf(a.sessionId as string, a.node as TreeNodeRef);
      if (cmd === "cancel_request") return true;
      throw new Error(`unexpected command ${cmd}`);
    });
    setDockviewApi({
      panels: [],
      addPanel: (options: { id: string; component: string; params: Record<string, unknown> }) => {
        added.push(options);
      },
      getPanel: () => undefined,
    } as never);
  });
  afterEach(() => {
    setDockviewApi(null);
    resetConnectionsStore();
  });

  async function expand(name: RegExp | string) {
    fireEvent.doubleClick(await screen.findByRole("treeitem", { name }));
  }

  it("opens a collection in the collection tab and offers the shell, not the designer", async () => {
    render(<ObjectTreePanel />);
    await act(async () => {
      await refreshSessions();
    });
    await expand(/^Mongo/);
    await expand(/oxbase_test/);
    await expand("Collections");
    const docs = await screen.findByRole("treeitem", { name: /m7_docs/ });
    fireEvent.doubleClick(docs);
    expect(added[0]).toMatchObject({
      component: "collection",
      params: { sessionId: "mongo", collection: DOCS },
    });
    fireEvent.click(docs);
    expect(screen.getByRole("button", { name: "Design" })).toBeDisabled();
    expect(screen.getByRole("button", { name: "New table" })).toBeDisabled();
    fireEvent.contextMenu(docs);
    const menu = screen.getByRole("menu");
    expect(within(menu).queryByRole("menuitem", { name: /Design/ })).toBeNull();
    fireEvent.click(within(menu).getByRole("menuitem", { name: /Mongo shell/ }));
    expect(added[1]).toMatchObject({
      component: "query",
      params: { sessionId: "mongo", database: "oxbase_test", title: "Shell oxbase_test" },
    });
  });

  it("opens Redis keys (binary ones by their bytes) in the key browser", async () => {
    render(<ObjectTreePanel />);
    await act(async () => {
      await refreshSessions();
    });
    await expand(/^Cache/);
    await expand(/^0/);
    await expand(/m7:$/);
    const hash = await screen.findByRole("treeitem", { name: /m7:hash/ });
    expect(hash).toHaveTextContent("hash · TTL 2 min");
    fireEvent.doubleClick(hash);
    expect(added[0]).toMatchObject({
      component: "keys",
      params: {
        sessionId: "redis",
        database: "0",
        openKey: { encoding: "utf8", text: "m7:hash" },
      },
    });
    fireEvent.doubleClick(screen.getByRole("treeitem", { name: /xff/ }));
    // The browser for database 0 is open already in a real layout; here a second one is added.
    expect(added[1]?.params.openKey).toEqual({ encoding: "hex", hex: "6d373aff" });

    fireEvent.contextMenu(screen.getByRole("treeitem", { name: /m7:$/ }));
    const menu = screen.getByRole("menu");
    fireEvent.click(within(menu).getByRole("menuitem", { name: /Browse these keys/ }));
    expect(added[2]).toMatchObject({ component: "keys", params: { pattern: "m7:*" } });

    fireEvent.contextMenu(screen.getByRole("treeitem", { name: /^Cache/ }));
    fireEvent.click(within(screen.getByRole("menu")).getByRole("menuitem", { name: /Console/ }));
    expect(added[3]).toMatchObject({ component: "console", params: { sessionId: "redis" } });
    fireEvent.contextMenu(screen.getByRole("treeitem", { name: /^Cache/ }));
    fireEvent.click(within(screen.getByRole("menu")).getByRole("menuitem", { name: /Pub\/Sub/ }));
    expect(added[4]).toMatchObject({ component: "pubsub", params: { sessionId: "redis" } });
  });
});
