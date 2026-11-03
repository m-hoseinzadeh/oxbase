import { act, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import type { TransferEvent, TransferPlan, TransferRequest } from "../../api";
import { refreshSessions, resetConnectionsStore } from "../../connections/store";
import {
  callsOf,
  channels,
  M8_SRC,
  MY_TARGET,
  MYSQL,
  mockCore,
  PG_SOURCE,
  requestOf,
  table,
} from "../../test/m8Fixtures";
import type { SqlEditorProps } from "../editor/SqlEditor";
import { resetSyncStore } from "./store";
import { TransferWizard } from "./TransferWizard";

vi.mock("@tauri-apps/api/core", async () => (await import("../../test/tauriMock")).tauriCore);
vi.mock("../editor/SqlEditor", () => ({
  SqlEditor: ({ value, testId }: SqlEditorProps) => <pre data-testid={testId}>{value}</pre>,
}));

const CUSTOMERS = table(M8_SRC, "customers");
const ORDERS = table(M8_SRC, "orders");

function planFor(request: TransferRequest): TransferPlan {
  const skip = new Set(
    (request.spec.overrides ?? []).filter((o) => o.skip).map((o) => `${o.object}:${o.column}`),
  );
  const mode = request.spec.options?.mode ?? "create";
  return {
    sourceEngine: "postgres",
    targetEngine: "mysql",
    tables: request.spec.objects.map((o, object) => {
      const name =
        o.type === "query"
          ? o.targetName
          : (o.targetName ?? (o.type === "table" ? o.table.name : o.view.name));
      return {
        object,
        target: { kind: "table", catalog: "m8_xfer_target", name },
        exists: mode !== "create",
        mode,
        columns: [
          {
            source: "id",
            sourceType: { type: "int", bits: 32, unsigned: false },
            sourceNative: "integer",
            target: "id",
            mapping: { dataType: { type: "int", bits: 32, unsigned: false }, fidelity: "exact" },
            overridden: false,
            skipped: false,
          },
          {
            source: "seen_at",
            sourceType: { type: "timestamp_tz", precision: 6 },
            sourceNative: "timestamp with time zone",
            target: "seen_at",
            mapping: {
              dataType: { type: "timestamp", precision: 6 },
              nativeType: "datetime(6)",
              fidelity: "narrowed",
              reason: "MySQL datetime keeps the UTC instant; the offset is dropped",
            },
            overridden: false,
            skipped: skip.has(`${object}:seen_at`),
          },
        ],
        fidelity: skip.has(`${object}:seen_at`) ? "exact" : "narrowed",
        ddl: [`CREATE TABLE \`${name}\` (\`id\` int NOT NULL)`],
        foreignKeys: object === 1 ? ["ALTER TABLE `orders` ADD CONSTRAINT fk FOREIGN KEY …"] : [],
        estimatedRows: 3,
      };
    }),
    order: request.spec.objects.map((_, i) => i).reverse(),
    fidelity: "narrowed",
    destructive: mode === "replace" || mode === "truncate",
    transactionalDdl: false,
    runnable: true,
  };
}

function setup() {
  mockCore({
    transfer_plan: (a) => planFor(a.request as TransferRequest),
    transfer_run: () => "xfer-1",
    transfer_cancel: () => true,
  });
}

async function renderWizard(onClose = vi.fn()) {
  await refreshSessions();
  render(
    <TransferWizard
      init={{ source: { sessionId: PG_SOURCE.id, container: M8_SRC }, objects: [CUSTOMERS] }}
      onClose={onClose}
    />,
  );
  await screen.findByTestId("transfer-objects");
  return onClose;
}

function next() {
  fireEvent.click(screen.getByTestId("transfer-next"));
}

async function toMapping() {
  await renderWizard();
  fireEvent.click(screen.getByRole("checkbox", { name: /m8_src\.orders/ }));
  next();
  fireEvent.change(screen.getByLabelText("Target connection"), { target: { value: MYSQL.id } });
  await waitFor(() =>
    expect(screen.getByLabelText("Target database")).toHaveValue("m8_xfer_target"),
  );
  await waitFor(() => expect(screen.getByTestId("transfer-next")).toBeEnabled());
  next();
  fireEvent.change(screen.getByLabelText("Target table of orders"), {
    target: { value: "orders_copy" },
  });
  next();
  await screen.findByTestId("transfer-mapping");
}

describe("TransferWizard", () => {
  beforeEach(() => {
    resetConnectionsStore();
    resetSyncStore();
    setup();
  });

  it("lists the source's tables and views with the entry point's table checked", async () => {
    await renderWizard();
    const list = screen.getByTestId("transfer-objects");
    expect(
      within(list)
        .getAllByRole("checkbox")
        .map((c) => c.parentElement?.textContent),
    ).toEqual([
      "m8_src.customers",
      "m8_src.m8_data",
      "m8_src.m8_nokey",
      "m8_src.orders",
      "m8_src.active_customersview",
    ]);
    expect(within(list).getByRole("checkbox", { name: /customers$/ })).toBeChecked();
    fireEvent.change(screen.getByLabelText("Filter tables by name"), { target: { value: "m8_" } });
    expect(within(list).getAllByRole("checkbox")).toHaveLength(2);
    fireEvent.click(screen.getByRole("button", { name: "Select shown" }));
    expect(screen.getByText("3 objects selected")).toBeInTheDocument();
  });

  it("plans the transfer and shows each column's mapping with its lossiness", async () => {
    await toMapping();
    await waitFor(() => expect(callsOf("transfer_plan").length).toBeGreaterThan(0));
    const request = callsOf("transfer_plan").at(-1)?.request as TransferRequest;
    expect(request).toMatchObject({
      sourceSession: PG_SOURCE.id,
      targetSession: MYSQL.id,
      spec: {
        objects: [
          { type: "table", table: CUSTOMERS },
          { type: "table", table: ORDERS, targetName: "orders_copy" },
        ],
        target: MY_TARGET,
        options: { mode: "create", batchRows: 10_000 },
      },
    });
    const rows = await screen.findAllByTestId("mapping-row");
    const seen = rows.find((r) => r.getAttribute("data-column") === "seen_at") as HTMLElement;
    const badge = within(seen).getByTestId("fidelity-badge");
    expect(badge).toHaveAttribute("data-fidelity", "narrowed");
    expect(badge).toHaveAttribute(
      "title",
      "MySQL datetime keeps the UTC instant; the offset is dropped",
    );
    expect(seen.querySelector(".transfer-table__type")).toHaveTextContent(
      "timestamp with time zone",
    );

    // Overrides: skip a column (re-planned), type a native type.
    fireEvent.click(within(seen).getByLabelText("Copy seen_at"));
    await waitFor(() =>
      expect(requestOf<TransferRequest>("transfer_plan", -1).spec.overrides).toEqual([
        { object: 0, column: "seen_at", skip: true },
      ]),
    );
    fireEvent.change(screen.getByLabelText("Native target type of id"), {
      target: { value: "bigint" },
    });
    await waitFor(() =>
      expect(requestOf<TransferRequest>("transfer_plan", -1).spec.overrides).toEqual([
        { object: 0, column: "id", nativeType: "bigint" },
        { object: 0, column: "seen_at", skip: true },
      ]),
    );
  });

  it("reviews the dry run and runs the transfer with per-table progress", async () => {
    await toMapping();
    await screen.findAllByTestId("mapping-row");
    next();
    fireEvent.change(screen.getByLabelText("On a bad row"), { target: { value: "continue" } });
    next();
    expect(await screen.findByTestId("transfer-ddl")).toHaveTextContent(
      "CREATE TABLE `orders_copy`",
    );
    expect(screen.getByTestId("transfer-order").textContent).toBe(
      "m8_xfer_target.orders_copym8_xfer_target.customers",
    );
    expect(screen.getByText("DDL is not transactional on the target")).toBeInTheDocument();
    const start = screen.getByTestId("transfer-start");
    await waitFor(() => expect(start).toBeEnabled());
    await act(async () => fireEvent.click(start));
    await waitFor(() => expect(callsOf("transfer_run")).toHaveLength(1));
    const request = callsOf("transfer_run")[0]?.request as TransferRequest;
    expect(request.spec.options).toMatchObject({ errors: "continue" });
    expect(request.confirmDestructive).toBe(false);

    const send = channels.transfer_run as (e: TransferEvent) => void;
    const plan = planFor(request);
    send({ type: "started", transferId: "xfer-1", plan });
    send({ type: "tableStarted", table: 1 });
    send({
      type: "progress",
      progress: {
        table: 1,
        rowsRead: 2,
        rowsWritten: 2,
        rowsCommitted: 0,
        rowsSkipped: 0,
        bytes: 2048,
        errors: 0,
        totalRows: 3,
        elapsedMs: 20,
      },
    });
    send({
      type: "rowError",
      table: 1,
      issue: { row: 3, column: "seen_at", message: "out of range" },
    });
    const rows = screen.getAllByTestId("transfer-table-row");
    expect(rows[0]).toHaveTextContent("Copying…");
    expect(rows[0]).toHaveTextContent("2 / 3");
    expect(screen.getByTestId("transfer-issues")).toHaveTextContent(
      "orders_copy row 3 (seen_at): out of range",
    );

    // Cancel goes to the core with the run's id.
    fireEvent.click(screen.getByTestId("transfer-cancel"));
    expect(callsOf("transfer_cancel")[0]).toEqual({ transferId: "xfer-1" });
    const report = (table: number, status: "done" | "cancelled" | "not_run") => ({
      table,
      target: plan.tables[table]?.target ?? MY_TARGET,
      status,
      progress: {
        table,
        rowsRead: 3,
        rowsWritten: status === "done" ? 3 : 0,
        rowsCommitted: 0,
        rowsSkipped: 0,
        bytes: 0,
        errors: 0,
        elapsedMs: 30,
      },
      issuesTruncated: false,
      leftBehind: status === "done" ? "" : "table `orders_copy` was dropped again",
    });
    send({
      type: "cancelled",
      report: {
        tables: [report(1, "cancelled"), report(0, "not_run")],
        foreignKeysAdded: 0,
        elapsedMs: 40,
      },
    });
    expect(await screen.findByTestId("transfer-result")).toHaveTextContent("Transfer cancelled");
    expect(screen.getAllByTestId("transfer-table-row")[0]).toHaveTextContent(
      "table `orders_copy` was dropped again",
    );
    expect(screen.getByRole("button", { name: "Close" })).toBeEnabled();
  });

  it("needs a confirmation when the mode overwrites target tables", async () => {
    await toMapping();
    await screen.findAllByTestId("mapping-row");
    next();
    fireEvent.change(screen.getByLabelText("When a target table exists"), {
      target: { value: "replace" },
    });
    expect(screen.getByText(/Destructive: existing target rows are lost/)).toBeInTheDocument();
    next();
    const start = await screen.findByTestId("transfer-start");
    await screen.findByText("This transfer overwrites target data");
    expect(start).toBeDisabled();
    fireEvent.click(screen.getByTestId("transfer-acknowledge"));
    await waitFor(() => expect(start).toBeEnabled());
    await act(async () => fireEvent.click(start));
    await waitFor(() => expect(callsOf("transfer_run")).toHaveLength(1));
    expect(requestOf<TransferRequest>("transfer_run", 0).confirmDestructive).toBe(true);
  });

  it("refuses a blocked plan", async () => {
    mockCore({
      transfer_plan: (a) => ({
        ...planFor(a.request as TransferRequest),
        runnable: false,
        blocked: ["m8_src.customers.c_point: no MySQL type for point"],
      }),
    });
    await toMapping();
    await screen.findAllByTestId("mapping-row");
    next();
    next();
    expect(await screen.findByTestId("transfer-blocked")).toHaveTextContent(
      "no MySQL type for point",
    );
    expect(screen.getByTestId("transfer-start")).toBeDisabled();
  });
});

describe("TransferWizard type report", () => {
  beforeEach(() => {
    resetConnectionsStore();
    resetSyncStore();
    mockCore({
      transfer_pair_report: () => ({
        source: "postgres",
        target: "mysql",
        rows: [
          {
            logical: { type: "uuid" },
            mapping: {
              dataType: { type: "char", length: 36 },
              nativeType: "char(36)",
              fidelity: "widened",
              reason: "MySQL has no uuid type: the text form is stored",
            },
          },
        ],
      }),
    });
  });

  it("shows how types map between the two engines on demand", async () => {
    await renderWizard();
    next();
    fireEvent.change(screen.getByLabelText("Target connection"), { target: { value: MYSQL.id } });
    const details = await screen.findByTestId("transfer-pair-report");
    expect(callsOf("transfer_pair_report")).toHaveLength(0);
    (details as HTMLDetailsElement).open = true;
    fireEvent(details, new Event("toggle"));
    await waitFor(() => expect(callsOf("transfer_pair_report")).toHaveLength(1));
    expect(callsOf("transfer_pair_report")[0]).toEqual({
      sourceSession: PG_SOURCE.id,
      targetSession: MYSQL.id,
    });
    expect(await within(details).findByText("char(36)")).toBeInTheDocument();
    expect(within(details).getByTestId("fidelity-badge")).toHaveAttribute(
      "title",
      "MySQL has no uuid type: the text form is stored",
    );
    expect(details).toHaveTextContent("How types map from PostgreSQL to MySQL");
  });
});
