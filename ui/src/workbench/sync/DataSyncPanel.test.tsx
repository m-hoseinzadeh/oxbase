import { act, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import type {
  DataApplyEvent,
  DataCompareEvent,
  DataCompareRequest,
  DataCompareResult,
  TableDataDiff,
} from "../../api";
import { refreshSessions, resetConnectionsStore } from "../../connections/store";
import {
  callsOf,
  channels,
  M8_SRC,
  M8_TGT,
  mockCore,
  PG_SOURCE,
  PG_TARGET,
  table,
} from "../../test/m8Fixtures";
import type { SqlEditorProps } from "../editor/SqlEditor";
import { DataSyncPanel } from "./DataSyncPanel";

vi.mock("@tauri-apps/api/core", async () => (await import("../../test/tauriMock")).tauriCore);
vi.mock("../editor/SqlEditor", () => ({
  SqlEditor: ({ value, testId }: SqlEditorProps) => <pre data-testid={testId}>{value}</pre>,
}));

function diff(pair: number, name: string, counts: [number, number, number]): TableDataDiff {
  return {
    pair,
    source: table(M8_SRC, name),
    target: table(M8_TGT, name),
    status: { type: "compared" },
    key: ["id"],
    columns: ["id", "name", "qty"],
    counts: {
      sourceRows: 20,
      targetRows: 20,
      same: 11,
      inserts: counts[0],
      updates: counts[1],
      deletes: counts[2],
    },
    samples:
      counts[0] + counts[1] + counts[2] === 0
        ? []
        : [
            { op: "insert", key: ["21"], source: ["21", "new row", "3"] },
            {
              op: "update",
              key: ["4"],
              source: ["4", "widget", "10"],
              target: ["4", "widget", "7"],
              changed: ["qty"],
            },
            { op: "delete", key: ["99"], target: ["99", "gone", null] },
          ],
    chunksSkipped: 0,
    elapsedMs: 15,
  };
}

const REFUSED: TableDataDiff = {
  ...diff(2, "m8_nokey", [0, 0, 0]),
  status: { type: "refused", reason: "m8_nokey has no primary or unique key." },
};

function result(id: string, inSync = false): DataCompareResult {
  return {
    compareId: id,
    tables: inSync
      ? [diff(0, "customers", [0, 0, 0]), diff(1, "m8_data", [0, 0, 0])]
      : [diff(0, "customers", [0, 0, 0]), diff(1, "m8_data", [2, 5, 2]), REFUSED],
    elapsedMs: 50,
  };
}

async function renderPanel() {
  mockCore({
    data_compare: () => "op-cmp",
    data_script: () => ({
      engine: "postgres",
      prologue: ["SET standard_conforming_strings = on;"],
      statements: ["INSERT INTO m8_tgt.m8_data (id, name) VALUES (21, 'new row')", "UPDATE x"],
      totalStatements: 7,
      truncated: false,
      destructive: false,
    }),
    data_apply: () => "op-apply",
    data_cancel: () => true,
  });
  await refreshSessions();
  render(
    <DataSyncPanel
      params={{
        source: { sessionId: PG_SOURCE.id, container: M8_SRC },
        target: { sessionId: PG_TARGET.id, container: M8_TGT },
      }}
    />,
  );
  await screen.findByTestId("data-pairs");
}

async function compareAll() {
  await renderPanel();
  fireEvent.click(screen.getByTestId("data-compare"));
  await waitFor(() => expect(channels.data_compare).toBeDefined());
  const send = channels.data_compare as (e: DataCompareEvent) => void;
  send({ type: "started", compareId: "cmp-1", tables: 3 });
  send({
    type: "progress",
    progress: {
      pair: 1,
      counts: { sourceRows: 10, targetRows: 9, same: 5, inserts: 1, updates: 2, deletes: 0 },
      chunks: 1,
      chunksSkipped: 0,
      elapsedMs: 5,
    },
  });
  send({ type: "finished", result: result("cmp-1") });
}

describe("DataSyncPanel", () => {
  beforeEach(() => {
    resetConnectionsStore();
  });

  it("pairs tables by name and compares the chosen pairs", async () => {
    await renderPanel();
    const rows = screen.getAllByTestId("data-pair");
    expect(rows.map((r) => r.getAttribute("data-table"))).toEqual([
      "customers",
      "m8_data",
      "m8_nokey",
      "orders",
    ]);
    // m8_nokey has no target table of that name: not compared.
    expect(within(rows[2] as HTMLElement).getByRole("checkbox")).not.toBeChecked();
    fireEvent.click(screen.getByLabelText("Compare orders"));
    fireEvent.change(screen.getByLabelText("Key columns of m8_data"), {
      target: { value: "id, name" },
    });
    fireEvent.click(screen.getByTestId("data-compare"));
    await waitFor(() => expect(callsOf("data_compare")).toHaveLength(1));
    const request = callsOf("data_compare")[0]?.request as DataCompareRequest;
    expect(request.sourceSession).toBe(PG_SOURCE.id);
    expect(request.targetSession).toBe(PG_TARGET.id);
    expect(request.pairs.map((p) => p.source.name)).toEqual(["customers", "m8_data"]);
    expect(request.pairs[1]).toMatchObject({
      target: table(M8_TGT, "m8_data"),
      key: { type: "columns", columns: ["id", "name"] },
    });
    expect(request.options).toEqual({ chunkRows: 10_000, sampleRows: 20, useChunkHash: true });
  });

  it("shows counts per table, refusals and sample rows with changed cells", async () => {
    await compareAll();
    const rows = await screen.findAllByTestId("data-table-row");
    const data = rows.find((r) => r.getAttribute("data-table") === "m8_data") as HTMLElement;
    expect(within(data).getByTestId("count-inserts")).toHaveTextContent("2");
    expect(within(data).getByTestId("count-updates")).toHaveTextContent("5");
    expect(within(data).getByTestId("count-deletes")).toHaveTextContent("2");
    const nokey = rows.find((r) => r.getAttribute("data-table") === "m8_nokey") as HTMLElement;
    expect(nokey).toHaveTextContent("Refused: m8_nokey has no primary or unique key.");
    expect(screen.getByTestId("data-totals")).toHaveTextContent(
      "2 inserts · 5 updates · 2 deletes",
    );

    fireEvent.click(within(data).getByRole("button", { name: "m8_src.m8_data" }));
    const samples = screen.getByTestId("data-samples");
    expect(within(samples).getByText("new row")).toBeInTheDocument();
    // The key is the first compared column: shown once.
    expect(
      within(samples)
        .getAllByRole("columnheader")
        .map((h) => h.textContent),
    ).toEqual(["id (key)", "name", "qty"]);
    expect(within(samples).getAllByText("21")).toHaveLength(1);
    fireEvent.click(within(samples).getByRole("tab", { name: "Updates (5)" }));
    const changed = samples.querySelector(".sync-cell--changed") as HTMLElement;
    expect(changed).toHaveTextContent("710");
    expect(changed.querySelector(".sync-cell__old")).toHaveTextContent("7");
    fireEvent.click(within(samples).getByRole("tab", { name: "Deletes (2)" }));
    expect(within(samples).getByText("NULL")).toBeInTheDocument();

    fireEvent.click(within(nokey).getByRole("button"));
    expect(screen.getByTestId("data-samples")).toHaveTextContent("Type key columns");
  });

  it("previews the script with deletes off by default", async () => {
    await compareAll();
    fireEvent.click(await screen.findByTestId("data-preview"));
    await waitFor(() => expect(callsOf("data_script")).toHaveLength(1));
    expect(callsOf("data_script")[0]?.request).toEqual({
      compareId: "cmp-1",
      options: { inserts: true, updates: true, deletes: false, previewLimit: 1000 },
    });
    expect(await screen.findByTestId("data-script")).toHaveTextContent(
      "INSERT INTO m8_tgt.m8_data (id, name) VALUES (21, 'new row');",
    );
    // Audit lead 4: the session the inlined literals need comes first.
    expect(screen.getByTestId("data-script").textContent).toBe(
      "SET standard_conforming_strings = on;\n" +
        "INSERT INTO m8_tgt.m8_data (id, name) VALUES (21, 'new row');\nUPDATE x;",
    );
    expect(screen.getByTestId("data-script-status")).toHaveTextContent("7 statements");
  });

  it("drops a preview when the insert, update or delete choices change", async () => {
    await compareAll();
    fireEvent.click(await screen.findByTestId("data-preview"));
    expect(await screen.findByTestId("data-script")).toBeInTheDocument();
    fireEvent.click(screen.getByTestId("data-deletes"));
    expect(screen.queryByTestId("data-script")).not.toBeInTheDocument();
    fireEvent.click(screen.getByTestId("data-preview"));
    await waitFor(() => expect(callsOf("data_script")).toHaveLength(2));
    expect(callsOf("data_script")[1]?.request).toMatchObject({
      options: { inserts: true, updates: true, deletes: true },
    });
  });

  it("names the compared target and waits for a new compare when a picker changes", async () => {
    await compareAll();
    fireEvent.click(await screen.findByTestId("data-apply"));
    const dialog = screen.getByRole("dialog", { name: "Apply the data changes?" });
    expect(within(dialog).getByTestId("data-confirm-target")).toHaveTextContent(
      "PG Target · oxbase_test.m8_tgt",
    );
    fireEvent.click(within(dialog).getByRole("button", { name: "Cancel" }));
    // Point the target picker elsewhere: the compare's apply would still hit m8_tgt.
    const schema = within(screen.getByTestId("data-target")).getByLabelText("Target schema");
    fireEvent.change(schema, { target: { value: "m8_src" } });
    expect(await screen.findByTestId("data-stale")).toHaveTextContent(
      "compare against PG Target · oxbase_test.m8_tgt",
    );
    expect(screen.queryByTestId("data-apply")).not.toBeInTheDocument();
    expect(screen.queryByTestId("data-preview")).not.toBeInTheDocument();
  });

  it("does not compare a schema with itself", async () => {
    await renderPanel();
    fireEvent.change(screen.getByLabelText("Target connection"), {
      target: { value: PG_SOURCE.id },
    });
    const schema = await within(screen.getByTestId("data-target")).findByLabelText("Target schema");
    await waitFor(() => expect(schema).toBeEnabled());
    fireEvent.change(schema, { target: { value: "m8_src" } });
    expect(await screen.findByTestId("data-same")).toBeInTheDocument();
    expect(screen.getByTestId("data-compare")).toBeDisabled();
  });

  it("needs its own confirmation for deletes, applies and shows the re-compare", async () => {
    await compareAll();
    fireEvent.click(await screen.findByTestId("data-deletes"));
    fireEvent.click(screen.getByTestId("data-apply"));
    const dialog = screen.getByRole("dialog", { name: "Apply the data changes?" });
    expect(dialog).toHaveTextContent("2 inserts, 5 updates, 2 deletes");
    const confirm = within(dialog).getByTestId("data-confirm");
    expect(confirm).toBeDisabled();
    fireEvent.click(within(dialog).getByTestId("data-acknowledge"));
    await act(async () => fireEvent.click(confirm));
    await waitFor(() => expect(callsOf("data_apply")).toHaveLength(1));
    expect(callsOf("data_apply")[0]?.request).toEqual({
      compareId: "cmp-1",
      options: {
        script: { inserts: true, updates: true, deletes: true },
        batchStatements: 1000,
        confirmDeletes: true,
      },
    });
    const send = channels.data_apply as (e: DataApplyEvent) => void;
    send({ type: "started", applyId: "op-apply", totalStatements: 9 });
    send({ type: "progress", statementsDone: 4, totalStatements: 9, pair: 1, elapsedMs: 3 });
    expect(screen.getByTestId("data-apply-progress")).toHaveTextContent("Applying 4 of 9");
    send({ type: "recomparing" });
    send({
      type: "finished",
      report: {
        tables: [
          {
            pair: 1,
            target: table(M8_TGT, "m8_data"),
            inserted: 2,
            updated: 5,
            deleted: 2,
          },
        ],
        statements: 9,
        committedBatches: 1,
        leftBehind: "",
        recompare: result("cmp-2", true),
        inSync: true,
        elapsedMs: 30,
      },
    });
    expect(await screen.findByTestId("data-result")).toHaveTextContent("In sync");
    expect(screen.getByTestId("data-totals")).toHaveTextContent(
      "0 inserts · 0 updates · 0 deletes",
    );
    await waitFor(() =>
      expect(callsOf("compare_forget").map((a) => a.compareId)).toContain("cmp-1"),
    );
  });

  it("cancels a running compare", async () => {
    await renderPanel();
    fireEvent.click(screen.getByTestId("data-compare"));
    const cancel = await screen.findByRole("button", { name: "Cancel" });
    await waitFor(() => expect(cancel).toBeEnabled());
    fireEvent.click(cancel);
    expect(callsOf("data_cancel")[0]).toEqual({ operationId: "op-cmp" });
    const send = channels.data_compare as (e: DataCompareEvent) => void;
    send({ type: "cancelled", result: { compareId: "cmp-x", tables: [], elapsedMs: 1 } });
    expect(await screen.findByText("Compare cancelled")).toBeInTheDocument();
  });
});
