import { act, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import type { ExportEvent, ExportRequest, FileFilter } from "../../api";
import { tauriCore } from "../../test/tauriMock";
import { ExportDialog, withFilter } from "./ExportDialog";
import {
  closeTransferDialog,
  type ExportInit,
  openExport,
  openImport,
  resetTransferStore,
} from "./store";
import { TransferHost } from "./TransferHost";

vi.mock("@tauri-apps/api/core", async () => (await import("../../test/tauriMock")).tauriCore);

interface Calls {
  saves: { defaultName: string; filters: FileFilter[] }[];
  runs: { file: string; request: ExportRequest }[];
  cancels: string[];
  released: string[];
}

let calls: Calls;
let send: (event: ExportEvent) => void;
let pickResult: "picked" | "cancelled";

function mockCore() {
  calls = { saves: [], runs: [], cancels: [], released: [] };
  send = () => {};
  pickResult = "picked";
  tauriCore.invoke.mockImplementation(async (cmd, args) => {
    const a = (args ?? {}) as Record<string, unknown>;
    switch (cmd) {
      case "file_pick_save":
        calls.saves.push({
          defaultName: a.defaultName as string,
          filters: a.filters as FileFilter[],
        });
        return pickResult === "cancelled"
          ? null
          : { handle: "w-1", name: a.defaultName, access: "write", exists: false };
      case "export_run": {
        calls.runs.push({ file: a.file as string, request: a.request as ExportRequest });
        const channel = a.onEvent as { onmessage: (e: ExportEvent) => void };
        send = (e) => act(() => channel.onmessage(e));
        return "exp-1";
      }
      case "export_cancel":
        calls.cancels.push(a.exportId as string);
        return true;
      case "file_release":
        calls.released.push(a.handle as string);
        return true;
      default:
        throw new Error(`unexpected command ${cmd}`);
    }
  });
}

const RESULT: ExportInit = {
  source: { type: "result", runId: "run-1", resultIndex: 0 },
  engine: "mysql",
  label: "Result 1",
  baseName: "result-1",
  totalRows: 4000,
};

const TABLE: ExportInit = {
  source: {
    type: "table",
    session: "s-1",
    query: { table: { kind: "table", schema: "public", name: "orders" } },
  },
  engine: "postgres",
  label: "Table public.orders",
  baseName: "orders",
  offerFilter: true,
};

describe("ExportDialog", () => {
  beforeEach(() => {
    tauriCore.invoke.mockReset();
    tauriCore.isTauri.mockReturnValue(true);
    mockCore();
    resetTransferStore();
  });

  it("warns that an empty NULL token hides empty text, and offers \\N", async () => {
    render(<ExportDialog init={RESULT} onClose={vi.fn()} />);
    expect(screen.getByTestId("null-token-hint")).toHaveTextContent(
      "NULL and empty text look the same",
    );
    fireEvent.click(screen.getByRole("button", { name: "Use \\N" }));
    expect(screen.getByLabelText("NULL is written as")).toHaveValue("\\N");
    expect(screen.queryByTestId("null-token-hint")).toBeNull();
    fireEvent.click(screen.getByTestId("start-export"));
    await waitFor(() => expect(calls.runs).toHaveLength(1));
    expect(calls.runs[0]?.request.options).toMatchObject({ format: "csv", nullToken: "\\N" });
  });

  it("exports a result: save dialog, progress, cancel", async () => {
    const onClose = vi.fn();
    render(<ExportDialog init={RESULT} onClose={onClose} />);
    expect(screen.getByTestId("export-source")).toHaveTextContent("Result 1 · 4,000 rows");
    fireEvent.change(screen.getByLabelText("Delimiter"), { target: { value: ";" } });
    fireEvent.click(screen.getByTestId("start-export"));
    await waitFor(() => expect(calls.runs).toHaveLength(1));
    expect(calls.saves[0]?.defaultName).toBe("result-1.csv");
    expect(calls.saves[0]?.filters[0]?.extensions).toContain("csv");
    expect(calls.runs[0]?.file).toBe("w-1");
    expect(calls.runs[0]?.request).toEqual({
      source: { type: "result", runId: "run-1", resultIndex: 0 },
      options: {
        format: "csv",
        delimiter: ";",
        header: true,
        quoteStyle: "necessary",
        encoding: "utf8",
        bom: false,
        lineEnding: "lf",
        nullToken: "",
        formulaGuard: true,
      },
    });

    send({ type: "started", exportId: "exp-1", totalRows: 4000 });
    send({ type: "progress", rows: 1000, bytesWritten: 50_000, totalRows: 4000, elapsedMs: 1000 });
    expect(screen.getByTestId("progress-done")).toHaveTextContent("25%");
    expect(screen.getByTestId("progress-rows")).toHaveTextContent("1,000 of 4,000");
    expect(screen.getByTestId("progress-written")).toHaveTextContent("50.0 KB");
    expect(screen.getByTestId("progress-rate")).toHaveTextContent(
      "1,000 rows/s · 50.0 KB/s written",
    );
    expect(screen.getByTestId("progress-time-left")).toHaveTextContent("about 3.00 s");

    fireEvent.keyDown(screen.getByRole("dialog"), { key: "Escape" });
    expect(onClose).not.toHaveBeenCalled();
    fireEvent.click(screen.getByTestId("cancel-export"));
    await waitFor(() => expect(calls.cancels).toEqual(["exp-1"]));
    send({ type: "cancelled", elapsedMs: 1200 });
    expect(screen.getByTestId("export-report")).toHaveTextContent(
      "Export cancelled: no file was written.",
    );
    fireEvent.click(screen.getByRole("button", { name: "Close" }));
    expect(onClose).toHaveBeenCalled();
  });

  it("writes a SQL dump for the source engine and reports the file", async () => {
    render(<ExportDialog init={RESULT} onClose={() => {}} />);
    fireEvent.change(screen.getByLabelText("Format"), { target: { value: "sql_dump" } });
    expect(screen.getByLabelText("SQL for")).toHaveValue("mysql");
    fireEvent.change(screen.getByLabelText("Table name in the dump"), {
      target: { value: "orders_copy" },
    });
    fireEvent.click(screen.getByLabelText("DROP TABLE first"));
    fireEvent.click(screen.getByTestId("start-export"));
    await waitFor(() => expect(calls.runs).toHaveLength(1));
    expect(calls.saves[0]?.defaultName).toBe("result-1.sql");
    expect(calls.runs[0]?.request.options).toEqual({
      format: "sql_dump",
      target: "mysql",
      includeCreate: true,
      includeDrop: true,
      rowsPerInsert: 100,
      table: { kind: "table", name: "orders_copy" },
    });
    send({
      type: "finished",
      report: {
        exportId: "exp-1",
        fileName: "result-1.sql",
        rows: 4000,
        bytes: 120_000,
        sourceTruncated: true,
        elapsedMs: 900,
      },
    });
    expect(screen.getByTestId("export-report")).toHaveTextContent("Export finished");
    expect(screen.getByTestId("report-file")).toHaveTextContent("result-1.sql");
    expect(screen.getByTestId("report-rows")).toHaveTextContent("4,000");
    expect(screen.getByText(/cut at the row limit/)).toBeInTheDocument();
  });

  it("checks options before the save dialog and stays put when it is cancelled", async () => {
    render(<ExportDialog init={RESULT} onClose={() => {}} />);
    fireEvent.change(screen.getByLabelText("Format"), { target: { value: "xlsx" } });
    fireEvent.change(screen.getByLabelText("Sheet name"), { target: { value: "a:b" } });
    fireEvent.click(screen.getByTestId("start-export"));
    expect(await screen.findByTestId("export-problems")).toHaveTextContent(
      "A sheet name cannot contain",
    );
    expect(calls.saves).toHaveLength(0);

    fireEvent.change(screen.getByLabelText("Sheet name"), { target: { value: "Orders" } });
    pickResult = "cancelled";
    fireEvent.click(screen.getByTestId("start-export"));
    await waitFor(() => expect(calls.saves).toHaveLength(1));
    expect(calls.saves[0]?.defaultName).toBe("result-1.xlsx");
    await waitFor(() => expect(screen.getByTestId("start-export")).toBeEnabled());
    expect(calls.runs).toHaveLength(0);
  });

  it("exports a table from the tree with a filter and a sort", async () => {
    render(<ExportDialog init={TABLE} onClose={() => {}} />);
    fireEvent.change(screen.getByLabelText("Format"), { target: { value: "parquet" } });
    fireEvent.change(screen.getByLabelText("Compression"), { target: { value: "zstd" } });
    fireEvent.change(screen.getByLabelText("Level"), { target: { value: "5" } });
    fireEvent.change(screen.getByLabelText("Filter (SQL condition)"), {
      target: { value: "amount > 10" },
    });
    fireEvent.change(screen.getByLabelText("Sort by column"), { target: { value: "id" } });
    fireEvent.click(screen.getByLabelText("Descending"));
    fireEvent.click(screen.getByTestId("start-export"));
    await waitFor(() => expect(calls.runs).toHaveLength(1));
    expect(calls.saves[0]?.defaultName).toBe("orders.parquet");
    expect(calls.runs[0]?.request).toEqual({
      source: {
        type: "table",
        session: "s-1",
        query: {
          table: { kind: "table", schema: "public", name: "orders" },
          filter: { type: "rawSql", sql: "amount > 10" },
          sort: [{ column: "id", descending: true }],
        },
      },
      options: { format: "parquet", compression: { codec: "zstd", level: 5 } },
    });
    send({ type: "failed", error: { category: "io", message: "disk full" }, elapsedMs: 10 });
    expect(screen.getByRole("alert")).toHaveTextContent("Export failed: no file was written.");
    expect(screen.getByRole("alert")).toHaveTextContent("disk full");
  });

  it("leaves a table query alone without a filter", () => {
    expect(withFilter(TABLE.source, " ", "", false)).toEqual(TABLE.source);
    expect(withFilter(RESULT.source, "x", "y", true)).toEqual(RESULT.source);
  });

  it("is opened through the store, which refuses a second dialog while one runs", async () => {
    render(<TransferHost />);
    expect(screen.queryByRole("dialog")).toBeNull();
    act(() => {
      openExport(RESULT);
    });
    expect(screen.getByRole("dialog", { name: "Export data" })).toBeInTheDocument();
    fireEvent.click(screen.getByTestId("start-export"));
    await waitFor(() => expect(calls.runs).toHaveLength(1));
    let opened = true;
    act(() => {
      opened = openImport();
    });
    expect(opened).toBe(false);
    expect(screen.getByRole("dialog", { name: "Export data" })).toBeInTheDocument();
    send({
      type: "finished",
      report: {
        exportId: "exp-1",
        fileName: "result-1.csv",
        rows: 1,
        bytes: 2,
        sourceTruncated: false,
        elapsedMs: 1,
      },
    });
    act(() => {
      opened = openImport();
    });
    expect(opened).toBe(true);
    expect(screen.getByRole("dialog", { name: "Import data" })).toBeInTheDocument();
    act(() => closeTransferDialog());
    expect(screen.queryByRole("dialog")).toBeNull();
  });
});
