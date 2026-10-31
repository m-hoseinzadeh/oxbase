import { tableFromArrays, tableToIPC } from "apache-arrow";
import { beforeEach, describe, expect, it, vi } from "vitest";

const core = vi.hoisted(() => {
  class Channel<T> {
    onmessage: (message: T) => void = () => {};
  }
  return {
    Channel,
    invoke: vi.fn<(cmd: string, args?: unknown) => Promise<unknown>>(),
    isTauri: vi.fn<() => boolean>(() => true),
  };
});
vi.mock("@tauri-apps/api/core", () => core);

import { isTerminalEvent, type RunEvent } from "./types";
import {
  historyClear,
  historyList,
  objectLoad,
  queryPage,
  queryRun,
  statementAt,
  treeChildren,
} from "./workbench";

describe("workbench command wrappers", () => {
  beforeEach(() => {
    core.invoke.mockReset();
    core.isTauri.mockReturnValue(true);
  });

  it("sends camelCase arguments with explicit nulls", async () => {
    core.invoke.mockResolvedValue([]);
    await treeChildren("s1", { type: "connection" });
    expect(core.invoke).toHaveBeenCalledWith("tree_children", {
      sessionId: "s1",
      node: { type: "connection" },
      options: null,
      requestId: null,
    });
    await objectLoad("s1", { kind: "table", catalog: "db", name: "t" }, "r1");
    expect(core.invoke).toHaveBeenLastCalledWith("object_load", {
      sessionId: "s1",
      object: { kind: "table", catalog: "db", name: "t" },
      requestId: "r1",
    });
    await historyList();
    expect(core.invoke).toHaveBeenLastCalledWith("history_list", { query: null });
    await historyClear("p1");
    expect(core.invoke).toHaveBeenLastCalledWith("history_clear", { profileId: "p1" });
    core.invoke.mockResolvedValue(null);
    expect(await statementAt("postgres", "SELECT 1", 3)).toBeNull();
  });

  it("passes run events through a channel and returns the run id", async () => {
    core.invoke.mockResolvedValue("run-1");
    const seen: RunEvent[] = [];
    const id = await queryRun("s1", "tab-1", "SELECT 1", (e) => seen.push(e), { maxRows: 10 });
    expect(id).toBe("run-1");
    const [cmd, args] = core.invoke.mock.calls[0] ?? [];
    expect(cmd).toBe("query_run");
    const { onEvent, ...rest } = args as { onEvent: { onmessage: (e: RunEvent) => void } };
    expect(rest).toEqual({
      sessionId: "s1",
      editorId: "tab-1",
      script: "SELECT 1",
      options: { maxRows: 10 },
    });
    onEvent.onmessage({ type: "finished", elapsedMs: 3 });
    expect(seen).toEqual([{ type: "finished", elapsedMs: 3 }]);
    expect(isTerminalEvent(seen[0] as RunEvent)).toBe(true);
    expect(isTerminalEvent({ type: "rows", resultIndex: 0, totalSoFar: 5 })).toBe(false);
  });

  it("decodes result pages from Arrow IPC bytes", async () => {
    const ipc = tableToIPC(tableFromArrays({ id: Int32Array.from([1, 2]) }), "stream");
    core.invoke.mockResolvedValue(ipc.slice().buffer);
    const page = await queryPage("run-1", 0, 0, 100);
    expect(page.numRows).toBe(2);
    expect(core.invoke).toHaveBeenCalledWith("query_page", {
      runId: "run-1",
      resultIndex: 0,
      offset: 0,
      limit: 100,
    });
  });

  it("normalises failures to command errors", async () => {
    core.invoke.mockRejectedValue({ category: "not_found", message: "no open session" });
    await expect(treeChildren("x", { type: "connection" })).rejects.toEqual({
      category: "not_found",
      message: "no open session",
    });
    core.isTauri.mockReturnValue(false);
    await expect(historyList()).rejects.toMatchObject({ category: "unsupported" });
  });
});
