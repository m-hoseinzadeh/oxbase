import { act, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { ObjectRef, TreeNode, TreeNodeRef } from "../../api";
import { refreshSessions, resetConnectionsStore } from "../../connections/store";
import type { SessionSummary } from "../../connections/types";
import { tauriCore } from "../../test/tauriMock";
import { setDockviewApi } from "../layout";
import { ObjectTreePanel } from "./ObjectTreePanel";

vi.mock("@tauri-apps/api/core", async () => (await import("../../test/tauriMock")).tauriCore);

const SESSION: SessionSummary = {
  id: "s-1",
  profile_id: "p-1",
  profile_name: "Local PG",
  engine: "postgres",
  info: {
    server_version: { major: 18, minor: 0, patch: 0, build: 0, raw: "18.0" },
    tls_in_use: false,
    via_ssh: false,
    latency_ms: 1,
  },
  opened_at: "2026-10-31T10:00:00Z",
};

const DB: ObjectRef = { kind: "database", name: "app" };
const USERS: ObjectRef = { kind: "table", catalog: "app", name: "users" };

function objectNode(object: ObjectRef, expandable: boolean, extra: Partial<TreeNode> = {}) {
  return {
    node: { type: "object", object },
    key: `object:${object.kind}:${object.name}`,
    label: object.name,
    expandable,
    summary: { ref: object, system: false },
    ...extra,
  } satisfies TreeNode;
}

function folder(parent: ObjectRef, kind: "table" | "column", label: string): TreeNode {
  return {
    node: { type: "folder", parent, kind },
    key: `folder:${kind}:${parent.name}`,
    label,
    expandable: true,
  };
}

/** The object tree of the mocked server. */
function childrenOf(node: TreeNodeRef, showSystem: boolean): TreeNode[] {
  if (node.type === "connection") {
    const dbs = [objectNode(DB, true)];
    if (showSystem) dbs.push(objectNode({ kind: "database", name: "template1" }, true));
    return dbs;
  }
  if (node.type === "object" && node.object.kind === "database") {
    return [folder(DB, "table", "Tables")];
  }
  if (node.type === "folder" && node.kind === "table") {
    return [
      objectNode(USERS, true, { summary: { ref: USERS, system: false, row_estimate: 12_000 } }),
      objectNode({ kind: "table", catalog: "app", name: "orders" }, true),
    ];
  }
  if (node.type === "object" && node.object.kind === "table") {
    return [folder(USERS, "column", "Columns")];
  }
  return [];
}

let treeHandler: (node: TreeNodeRef, showSystem: boolean) => Promise<TreeNode[]>;

function mockCore() {
  tauriCore.invoke.mockImplementation(async (cmd, args) => {
    const a = (args ?? {}) as Record<string, unknown>;
    switch (cmd) {
      case "list_open_connections":
        return [SESSION];
      case "tree_children": {
        const options = a.options as { showSystem?: boolean } | null;
        return treeHandler(a.node as TreeNodeRef, options?.showSystem ?? false);
      }
      case "cancel_request":
        return true;
      default:
        throw new Error(`unexpected command ${cmd}`);
    }
  });
}

function treeCalls(): Array<{ node: TreeNodeRef; requestId: unknown; options: unknown }> {
  return tauriCore.invoke.mock.calls
    .filter(([cmd]) => cmd === "tree_children")
    .map(([, args]) => args as { node: TreeNodeRef; requestId: unknown; options: unknown });
}

async function renderTree() {
  render(<ObjectTreePanel />);
  await act(async () => {
    await refreshSessions();
  });
  return screen.findByRole("tree", { name: "Database objects" });
}

function item(name: string | RegExp): HTMLElement {
  return screen.getByRole("treeitem", { name });
}

describe("ObjectTreePanel", () => {
  beforeEach(() => {
    resetConnectionsStore();
    tauriCore.invoke.mockReset();
    tauriCore.isTauri.mockReturnValue(true);
    treeHandler = async (node, showSystem) => childrenOf(node, showSystem);
    mockCore();
  });
  afterEach(() => {
    resetConnectionsStore();
  });

  it("loads children only when a node is expanded", async () => {
    const tree = await renderTree();
    const root = item(/Local PG/);
    expect(root).toHaveAttribute("aria-expanded", "false");
    expect(treeCalls()).toHaveLength(0);

    fireEvent.click(root);
    fireEvent.keyDown(tree, { key: "ArrowRight" });
    expect(await screen.findByRole("treeitem", { name: /app/ })).toBeInTheDocument();
    expect(treeCalls()).toHaveLength(1);
    expect(treeCalls()[0]).toMatchObject({
      node: { type: "connection" },
      options: { showSystem: false },
    });
    expect(typeof treeCalls()[0]?.requestId).toBe("string");

    // Expanding the database lists its folders; the folder itself is not read yet.
    fireEvent.doubleClick(item(/app/));
    expect(await screen.findByRole("treeitem", { name: "Tables" })).toBeInTheDocument();
    expect(treeCalls()).toHaveLength(2);
    expect(treeCalls()[1]?.node).toEqual({ type: "object", object: DB });

    fireEvent.doubleClick(item("Tables"));
    const users = await screen.findByRole("treeitem", { name: /users/ });
    expect(users).toHaveTextContent("~12k rows");
    expect(users).toHaveAttribute("aria-level", "4");
    expect(treeCalls()).toHaveLength(3);
  });

  it("navigates with the keyboard", async () => {
    const tree = await renderTree();
    fireEvent.keyDown(tree, { key: "ArrowDown" });
    expect(item(/Local PG/)).toHaveAttribute("aria-selected", "true");
    fireEvent.keyDown(tree, { key: "ArrowRight" });
    await screen.findByRole("treeitem", { name: /app/ });
    fireEvent.keyDown(tree, { key: "ArrowRight" }); // to the first child
    expect(item(/app/)).toHaveAttribute("aria-selected", "true");
    expect(tree).toHaveAttribute("aria-activedescendant", item(/app/).id);
    fireEvent.keyDown(tree, { key: "Enter" }); // expand
    await screen.findByRole("treeitem", { name: "Tables" });
    fireEvent.keyDown(tree, { key: "End" });
    expect(item("Tables")).toHaveAttribute("aria-selected", "true");
    fireEvent.keyDown(tree, { key: "ArrowLeft" }); // to the parent
    expect(item(/app/)).toHaveAttribute("aria-selected", "true");
    fireEvent.keyDown(tree, { key: "ArrowLeft" }); // collapse
    expect(item(/app/)).toHaveAttribute("aria-expanded", "false");
    expect(screen.queryByRole("treeitem", { name: "Tables" })).not.toBeInTheDocument();
    fireEvent.keyDown(tree, { key: "Home" });
    expect(item(/Local PG/)).toHaveAttribute("aria-selected", "true");
  });

  it("shows loading and error states, and retries", async () => {
    let fail = true;
    let release: (() => void) | null = null;
    treeHandler = (node, showSystem) => {
      if (node.type !== "connection") return Promise.resolve(childrenOf(node, showSystem));
      return new Promise((resolve, reject) => {
        release = () =>
          fail
            ? reject({ category: "connection", message: "server closed the connection" })
            : resolve(childrenOf(node, showSystem));
      });
    };
    const tree = await renderTree();
    fireEvent.click(item(/Local PG/));
    fireEvent.keyDown(tree, { key: "ArrowRight" });
    expect(await screen.findByText("Loading…")).toBeInTheDocument();
    await act(async () => release?.());
    const error = await screen.findByRole("treeitem", { name: /Could not load/ });
    expect(error).toHaveTextContent("server closed the connection");

    fail = false;
    fireEvent.click(within(error).getByRole("button", { name: "Retry" }));
    await waitFor(() => expect(treeCalls()).toHaveLength(2));
    await act(async () => release?.());
    expect(await screen.findByRole("treeitem", { name: /app/ })).toBeInTheDocument();
  });

  it("cancels the request of a node collapsed while loading", async () => {
    treeHandler = () => new Promise(() => {});
    const tree = await renderTree();
    fireEvent.click(item(/Local PG/));
    fireEvent.keyDown(tree, { key: "ArrowRight" });
    await screen.findByText("Loading…");
    const requestId = treeCalls()[0]?.requestId;
    fireEvent.keyDown(tree, { key: "ArrowLeft" });
    await waitFor(() =>
      expect(tauriCore.invoke).toHaveBeenCalledWith("cancel_request", { requestId }),
    );
    expect(screen.queryByText("Loading…")).not.toBeInTheDocument();
  });

  it("refreshes the selected node and lists system objects on request", async () => {
    const tree = await renderTree();
    fireEvent.click(item(/Local PG/));
    fireEvent.keyDown(tree, { key: "ArrowRight" });
    await screen.findByRole("treeitem", { name: /app/ });
    fireEvent.click(screen.getByRole("button", { name: "Refresh" }));
    await waitFor(() => expect(treeCalls()).toHaveLength(2));
    expect(treeCalls()[1]?.node).toEqual({ type: "connection" });

    fireEvent.click(screen.getByRole("checkbox", { name: "System objects" }));
    expect(await screen.findByRole("treeitem", { name: /template1/ })).toBeInTheDocument();
    expect(treeCalls()[2]?.options).toEqual({ showSystem: true });
  });

  it("asks for a connection when none is open", async () => {
    tauriCore.invoke.mockResolvedValue([]);
    render(<ObjectTreePanel />);
    expect(screen.getByText("No open connections")).toBeInTheDocument();
  });
  it("opens a table's data on double-click and its designer from the toolbar", async () => {
    const added: Array<{ id: string; component: string; params: unknown }> = [];
    const api = {
      panels: [] as unknown[],
      addPanel: (options: { id: string; component: string; params: unknown }) => {
        added.push(options);
      },
      getPanel: () => undefined,
    };
    setDockviewApi(api as never);
    try {
      const tree = await renderTree();
      fireEvent.click(item(/Local PG/));
      fireEvent.keyDown(tree, { key: "ArrowRight" });
      fireEvent.doubleClick(await screen.findByRole("treeitem", { name: /app/ }));
      fireEvent.doubleClick(await screen.findByRole("treeitem", { name: "Tables" }));
      const users = await screen.findByRole("treeitem", { name: /users/ });

      // A table double-click opens its rows instead of expanding it.
      fireEvent.doubleClick(users);
      expect(users).toHaveAttribute("aria-expanded", "false");
      expect(added).toHaveLength(1);
      expect(added[0]).toMatchObject({
        component: "tableData",
        params: { sessionId: "s-1", table: USERS },
      });
      expect(added[0]?.id).toMatch(/^data-editor-/);

      fireEvent.click(users);
      fireEvent.click(screen.getByRole("button", { name: "Design" }));
      expect(added[1]).toMatchObject({
        component: "designer",
        params: { sessionId: "s-1", table: USERS },
      });

      // New table goes into the selected table's database.
      fireEvent.click(screen.getByRole("button", { name: "New table" }));
      expect(added[2]).toMatchObject({
        component: "designer",
        params: {
          sessionId: "s-1",
          table: { kind: "table", catalog: "app", name: "" },
          isNew: true,
        },
      });

      // Columns and folders have no data to open.
      fireEvent.click(item("Tables"));
      expect(screen.getByRole("button", { name: "Open" })).toBeDisabled();
    } finally {
      setDockviewApi(null);
    }
  });
});
