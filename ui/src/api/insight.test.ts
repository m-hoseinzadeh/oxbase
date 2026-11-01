import { beforeEach, describe, expect, it, vi } from "vitest";

const core = vi.hoisted(() => ({
  invoke: vi.fn<(cmd: string, args?: unknown) => Promise<unknown>>(),
  isTauri: vi.fn<() => boolean>(() => true),
}));
vi.mock("@tauri-apps/api/core", () => core);

import {
  type CompletionResponse,
  formatSql,
  type InsightRequest,
  InsightSequencer,
  insightCloseEditor,
  insightComplete,
  insightDiagnose,
  insightHover,
  schemaCacheRefresh,
  schemaCacheStatus,
  snippetDelete,
  snippetList,
  snippetSave,
} from "./insight";

const request: InsightRequest = {
  editorId: "tab-1",
  seq: 3,
  engine: "postgres",
  sessionId: "s1",
  database: null,
  text: "SELECT * FROM ",
  offset: 14,
};

describe("insight command wrappers", () => {
  beforeEach(() => {
    core.invoke.mockReset();
    core.isTauri.mockReturnValue(true);
  });

  it("sends the request object with explicit nulls", async () => {
    const answer: CompletionResponse = {
      seq: 3,
      superseded: false,
      items: [],
      incomplete: false,
      cache: "ready",
      elapsedUs: 120,
    };
    core.invoke.mockResolvedValue(answer);
    expect(await insightComplete(request)).toEqual(answer);
    expect(core.invoke).toHaveBeenLastCalledWith("insight_complete", {
      request,
      limit: null,
      requestId: null,
    });
    await insightComplete(request, 50, "r1");
    expect(core.invoke).toHaveBeenLastCalledWith("insight_complete", {
      request,
      limit: 50,
      requestId: "r1",
    });
    await insightDiagnose(request);
    expect(core.invoke).toHaveBeenLastCalledWith("insight_diagnose", {
      request,
      requestId: null,
    });
    core.invoke.mockResolvedValue(null);
    expect(await insightHover(request)).toBeNull();
    expect(core.invoke).toHaveBeenLastCalledWith("insight_hover", { request });
    await insightCloseEditor("tab-1");
    expect(core.invoke).toHaveBeenLastCalledWith("insight_close_editor", { editorId: "tab-1" });
  });

  it("formats, refreshes caches and manages snippets", async () => {
    core.invoke.mockResolvedValue({});
    await formatSql({ engine: "mssql", text: "select 1", range: null, options: null });
    expect(core.invoke).toHaveBeenLastCalledWith("format_sql", {
      request: { engine: "mssql", text: "select 1", range: null, options: null },
    });
    await schemaCacheRefresh("s1");
    expect(core.invoke).toHaveBeenLastCalledWith("schema_cache_refresh", {
      sessionId: "s1",
      database: null,
      scope: null,
      requestId: null,
    });
    await schemaCacheRefresh("s1", "app", { type: "schemas", names: ["public"] }, "r2");
    expect(core.invoke).toHaveBeenLastCalledWith("schema_cache_refresh", {
      sessionId: "s1",
      database: "app",
      scope: { type: "schemas", names: ["public"] },
      requestId: "r2",
    });
    await schemaCacheStatus("s1");
    expect(core.invoke).toHaveBeenLastCalledWith("schema_cache_status", {
      sessionId: "s1",
      database: null,
    });
    core.invoke.mockResolvedValue([]);
    await snippetList();
    expect(core.invoke).toHaveBeenLastCalledWith("snippet_list", { engine: null });
    await snippetList("sqlite");
    expect(core.invoke).toHaveBeenLastCalledWith("snippet_list", { engine: "sqlite" });
    const input = { name: "Mine", prefix: "mine", body: "SELECT $0" };
    await snippetSave(input);
    expect(core.invoke).toHaveBeenLastCalledWith("snippet_save", { snippet: input });
    core.invoke.mockResolvedValue(true);
    expect(await snippetDelete("u1")).toBe(true);
    expect(core.invoke).toHaveBeenLastCalledWith("snippet_delete", { id: "u1" });
  });

  it("normalises failures to command errors", async () => {
    core.invoke.mockRejectedValue({ category: "invalid_input", message: "bad prefix" });
    await expect(snippetSave({ name: "x", prefix: "a b", body: "x" })).rejects.toMatchObject({
      category: "invalid_input",
      message: "bad prefix",
    });
  });
});

describe("InsightSequencer", () => {
  it("numbers requests per editor and kind and spots stale answers", () => {
    const seq = new InsightSequencer();
    expect(seq.next("a", "complete")).toBe(1);
    expect(seq.next("a", "complete")).toBe(2);
    expect(seq.next("a", "diagnose")).toBe(1);
    expect(seq.next("b", "complete")).toBe(1);
    expect(seq.isStale("a", "complete", { seq: 1, superseded: false })).toBe(true);
    expect(seq.isStale("a", "complete", { seq: 2, superseded: false })).toBe(false);
    expect(seq.isStale("a", "complete", { seq: 2, superseded: true })).toBe(true);
    seq.forget("a");
    expect(seq.next("a", "complete")).toBe(1);
    expect(seq.next("b", "complete")).toBe(2);
  });
});
