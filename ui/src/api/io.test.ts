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

import {
  type ExportEvent,
  type ExportRequest,
  exportCancel,
  exportRun,
  filePickOpen,
  filePickSave,
  fileRelease,
  filtersFor,
  type ImportEvent,
  type ImportRequest,
  importCancel,
  importPlan,
  importPreview,
  importRun,
  isTerminalTransferEvent,
} from "./io";

const request: ImportRequest = {
  options: { format: "csv", delimiter: ";", encoding: "windows1252" },
  columns: [
    { name: "id", dataType: { type: "int", bits: 64, unsigned: false } },
    { name: "note", dataType: { type: "text" } },
  ],
  mapping: [
    { source: 0, target: "id" },
    { source: 1, target: "note" },
  ],
  target: {
    session: "s1",
    table: { kind: "table", catalog: "db", schema: "public", name: "t" },
    mode: "append",
  },
  errors: { type: "skip", maxErrors: 10 },
  transaction: { type: "batches", rows: 100000 },
};

describe("file handle wrappers", () => {
  beforeEach(() => {
    core.invoke.mockReset();
    core.isTauri.mockReturnValue(true);
  });

  it("asks Rust for the dialogs and never sends a path", async () => {
    core.invoke.mockResolvedValue({
      handle: "h1",
      name: "orders.csv",
      access: "read",
      size: 12,
      exists: true,
    });
    const picked = await filePickOpen(filtersFor("csv"));
    expect(picked?.handle).toBe("h1");
    expect(core.invoke).toHaveBeenLastCalledWith("file_pick_open", {
      filters: [{ name: "Delimited text", extensions: ["csv", "tsv", "txt"] }],
      title: null,
    });

    core.invoke.mockResolvedValue(null);
    expect(await filePickSave("orders.parquet", filtersFor("parquet"), "Export")).toBeNull();
    expect(core.invoke).toHaveBeenLastCalledWith("file_pick_save", {
      defaultName: "orders.parquet",
      filters: [{ name: "Parquet", extensions: ["parquet"] }],
      title: "Export",
    });

    core.invoke.mockResolvedValue(true);
    expect(await fileRelease("h1")).toBe(true);
    expect(core.invoke).toHaveBeenLastCalledWith("file_release", { handle: "h1" });
  });

  it("has filters for every format", () => {
    for (const format of ["csv", "json", "xlsx", "parquet", "sql_dump"] as const) {
      const [filter] = filtersFor(format);
      expect(filter?.extensions.length).toBeGreaterThan(0);
      for (const ext of filter?.extensions ?? []) expect(ext).toMatch(/^[a-z0-9]+$/);
    }
  });
});

describe("import and export wrappers", () => {
  beforeEach(() => {
    core.invoke.mockReset();
    core.isTauri.mockReturnValue(true);
  });

  it("previews and plans with a file handle", async () => {
    core.invoke.mockResolvedValue({});
    await importPreview("h1", { format: "xlsx", sheet: "Data", headerRow: 2 });
    expect(core.invoke).toHaveBeenLastCalledWith("import_preview", {
      file: "h1",
      options: { format: "xlsx", sheet: "Data", headerRow: 2 },
      requestId: null,
    });
    await importPlan("h1", request, "r1");
    expect(core.invoke).toHaveBeenLastCalledWith("import_plan", {
      file: "h1",
      request,
      requestId: "r1",
    });
  });

  it("runs an import with a channel", async () => {
    core.invoke.mockResolvedValue("imp-1");
    const seen: ImportEvent[] = [];
    expect(await importRun("h1", request, (e) => seen.push(e))).toBe("imp-1");
    const [cmd, args] = core.invoke.mock.calls[0] ?? [];
    expect(cmd).toBe("import_run");
    const { onEvent, ...rest } = args as { onEvent: { onmessage: (e: ImportEvent) => void } };
    expect(rest).toEqual({ file: "h1", request });
    onEvent.onmessage({ type: "committed", rowsCommitted: 100000 });
    expect(seen).toEqual([{ type: "committed", rowsCommitted: 100000 }]);
    expect(isTerminalTransferEvent(seen[0] as ImportEvent)).toBe(false);

    core.invoke.mockResolvedValue(true);
    await importCancel("imp-1");
    expect(core.invoke).toHaveBeenLastCalledWith("import_cancel", { importId: "imp-1" });
  });

  it("runs an export of a result with a channel", async () => {
    core.invoke.mockResolvedValue("exp-1");
    const exportRequest: ExportRequest = {
      source: { type: "result", runId: "run-1", resultIndex: 0 },
      options: { format: "parquet", compression: { codec: "zstd", level: 3 } },
    };
    const seen: ExportEvent[] = [];
    expect(await exportRun("h2", exportRequest, (e) => seen.push(e))).toBe("exp-1");
    const [cmd, args] = core.invoke.mock.calls[0] ?? [];
    expect(cmd).toBe("export_run");
    const { onEvent, ...rest } = args as { onEvent: { onmessage: (e: ExportEvent) => void } };
    expect(rest).toEqual({ file: "h2", request: exportRequest });
    onEvent.onmessage({ type: "cancelled", elapsedMs: 3 });
    expect(seen).toHaveLength(1);
    expect(isTerminalTransferEvent(seen[0] as ExportEvent)).toBe(true);

    core.invoke.mockResolvedValue(false);
    expect(await exportCancel("exp-1")).toBe(false);
    expect(core.invoke).toHaveBeenLastCalledWith("export_cancel", { exportId: "exp-1" });
  });
});
