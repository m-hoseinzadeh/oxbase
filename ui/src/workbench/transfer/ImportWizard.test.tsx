import { act, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import type {
  ImportEvent,
  ImportPlan,
  ImportReport,
  ImportRequest,
  ObjectDef,
  PickedFile,
  SourcePreview,
} from "../../api";
import { refreshSessions, resetConnectionsStore } from "../../connections/store";
import type { SessionSummary } from "../../connections/types";
import { tauriCore } from "../../test/tauriMock";
import { ImportWizard } from "./ImportWizard";
import type { ImportInit } from "./store";

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

const PICKED: PickedFile = {
  handle: "h-1",
  name: "Orders 2026.csv",
  access: "read",
  size: 2_000_000,
  exists: true,
};

const PREVIEW: SourcePreview = {
  columns: [
    { index: 0, name: "id", dataType: { type: "int", bits: 64, unsigned: false }, nullable: false },
    {
      index: 1,
      name: "amount",
      dataType: { type: "decimal", precision: 10, scale: 2 },
      nullable: true,
      warnings: ['"n/a" in row 7 is not a decimal'],
    },
    { index: 2, name: "note", dataType: { type: "text" }, nullable: true },
  ],
  rows: [
    ["1", "9.50", "first"],
    ["2", null, "second"],
  ],
  sampledRows: 2,
  complete: true,
  totalBytes: 2_000_000,
  detected: { encoding: "utf8", bom: false, delimiter: "," },
};

const ORDERS: ObjectDef = {
  kind: "table",
  ref: { kind: "table", catalog: "app", schema: "public", name: "orders" },
  columns: [
    {
      name: "id",
      data_type: { type: "int", bits: 64, unsigned: false },
      native_type: "bigint",
      nullable: false,
      auto_increment: false,
      extras: {},
    },
    {
      name: "amount",
      data_type: { type: "decimal", precision: 10, scale: 2 },
      native_type: "numeric(10,2)",
      nullable: true,
      auto_increment: false,
      extras: {},
    },
    {
      name: "note",
      data_type: { type: "text" },
      native_type: "text",
      nullable: true,
      auto_increment: false,
      extras: {},
    },
    {
      name: "created_at",
      data_type: { type: "timestamp", precision: 6 },
      native_type: "timestamp",
      nullable: false,
      auto_increment: false,
      extras: {},
    },
  ],
  indexes: [],
  foreign_keys: [],
  checks: [],
  extras: {},
};

function plan(request: ImportRequest, extra: Partial<ImportPlan> = {}): ImportPlan {
  return {
    table: request.target.table,
    mode: request.target.mode ?? "append",
    createsTable: request.target.mode === "create",
    ddl: request.target.mode === "create" ? ['CREATE TABLE "public"."orders_2026" (...)'] : [],
    columns: request.mapping.map((m) => ({
      source: m.source,
      sourceName: request.columns[m.source]?.name ?? "",
      sourceType: request.columns[m.source]?.dataType ?? { type: "text" },
      target: m.target ?? "",
      targetType: m.dataType ?? { type: "text" },
      ...(m.source === 1 ? { conversion: "decimal text, exact" } : {}),
    })),
    method: "copy_text",
    transaction: {},
    destructive: false,
    ...extra,
  };
}

function report(extra: Partial<ImportReport> = {}): ImportReport {
  return {
    importId: "imp-1",
    table: { kind: "table", schema: "public", name: "orders_2026" },
    method: "copy_text",
    rowsRead: 10_000,
    rowsInserted: 9_000,
    rowsCommitted: 0,
    rowsSkipped: 2,
    errors: [
      { row: 7, column: "amount", message: '"n/a" is not a decimal' },
      { row: 9, column: "id", message: "out of range" },
    ],
    errorsTruncated: false,
    tableCreated: true,
    tableDropped: true,
    leftBehind: "The import was rolled back: the new table is gone.",
    bytesRead: 1_000_000,
    elapsedMs: 2000,
    ...extra,
  };
}

interface Calls {
  previews: { options: unknown }[];
  plans: ImportRequest[];
  planIds: (string | null)[];
  cancelledRequests: string[];
  runs: ImportRequest[];
  cancels: string[];
  released: string[];
  loads: unknown[];
}

let calls: Calls;
let send: (event: ImportEvent) => void;
let planExtra: Partial<ImportPlan>;
let previewFails: boolean;
/** When set, `import_plan` answers only when the test resolves it. */
let deferPlans: boolean;
let pendingPlans: { request: ImportRequest; resolve: (extra: Partial<ImportPlan>) => void }[];
let pickGate: Promise<PickedFile | null> | null;

function mockCore() {
  calls = {
    previews: [],
    plans: [],
    planIds: [],
    cancelledRequests: [],
    runs: [],
    cancels: [],
    released: [],
    loads: [],
  };
  send = () => {};
  planExtra = {};
  previewFails = false;
  deferPlans = false;
  pendingPlans = [];
  pickGate = null;
  tauriCore.invoke.mockImplementation(async (cmd, args) => {
    const a = (args ?? {}) as Record<string, unknown>;
    switch (cmd) {
      case "list_open_connections":
        return [SESSION];
      case "file_pick_open":
        return pickGate ?? PICKED;
      case "file_release":
        calls.released.push(a.handle as string);
        return true;
      case "import_preview":
        calls.previews.push({ options: a.options });
        if (previewFails)
          throw { category: "invalid_input", message: "line 3: unterminated quote" };
        return PREVIEW;
      case "cancel_request":
        calls.cancelledRequests.push(a.requestId as string);
        return true;
      case "object_load":
        calls.loads.push(a.object);
        return ORDERS;
      case "import_plan": {
        const request = a.request as ImportRequest;
        calls.plans.push(request);
        calls.planIds.push((a.requestId as string | null) ?? null);
        if (!deferPlans) return plan(request, planExtra);
        return new Promise<ImportPlan>((resolve) => {
          pendingPlans.push({ request, resolve: (extra) => resolve(plan(request, extra)) });
        });
      }
      case "import_run": {
        calls.runs.push(a.request as ImportRequest);
        const channel = a.onEvent as { onmessage: (e: ImportEvent) => void };
        send = (e) => act(() => channel.onmessage(e));
        return "imp-1";
      }
      case "import_cancel":
        calls.cancels.push(a.importId as string);
        return true;
      default:
        throw new Error(`unexpected command ${cmd}`);
    }
  });
}

async function renderWizard(init: ImportInit = {}) {
  await refreshSessions();
  const onClose = vi.fn();
  const view = render(<ImportWizard init={init} onClose={onClose} />);
  return { ...view, onClose };
}

function next() {
  fireEvent.click(screen.getByRole("button", { name: "Next" }));
}

async function pickAndPreview() {
  fireEvent.click(screen.getByRole("button", { name: "Choose file…" }));
  await screen.findByTestId("preview-grid");
}

async function toMapping() {
  await pickAndPreview();
  next();
  await screen.findByLabelText("Table");
  next();
  await screen.findByRole("table", { name: "Column mapping" });
}

const MODE_TEXT = { truncate: "Empty the table first (TRUNCATE)" } as const;

/** The default mock, with `object_load` answering `table`. */
function wrapLoad(table: ObjectDef) {
  const base = tauriCore.invoke.getMockImplementation();
  return async (cmd: string, args?: unknown) => {
    if (cmd === "object_load") {
      calls.loads.push((args as Record<string, unknown>).object);
      return table;
    }
    return base?.(cmd, args as never);
  };
}

describe("ImportWizard", () => {
  beforeEach(() => {
    resetConnectionsStore();
    tauriCore.invoke.mockReset();
    tauriCore.isTauri.mockReturnValue(true);
    mockCore();
  });

  it("previews the picked file with inferred types and follows option changes", async () => {
    await renderWizard();
    expect(screen.getByRole("button", { name: "Next" })).toBeDisabled();
    await pickAndPreview();
    expect(calls.previews[0]?.options).toEqual({ format: "csv", delimiter: "," });
    const grid = screen.getByTestId("preview-grid");
    expect(
      within(grid)
        .getAllByTestId("preview-type")
        .map((e) => e.textContent),
    ).toEqual(["bigint", "decimal(10, 2)", "text"]);
    expect(within(grid).getByText("second")).toBeInTheDocument();
    expect(within(grid).getByText("NULL")).toBeInTheDocument();
    expect(screen.getByTestId("preview-status")).toHaveTextContent("3 columns");
    expect(screen.getByText('amount: "n/a" in row 7 is not a decimal')).toBeInTheDocument();

    fireEvent.change(screen.getByLabelText("Delimiter"), { target: { value: ";" } });
    await waitFor(() => expect(calls.previews).toHaveLength(2));
    expect(calls.previews[1]?.options).toEqual({ format: "csv", delimiter: ";" });

    // A blank custom delimiter is not sent.
    fireEvent.change(screen.getByLabelText("Delimiter"), { target: { value: "other" } });
    expect(await screen.findByText("Enter a one-character delimiter.")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Next" })).toBeDisabled();

    fireEvent.change(screen.getByLabelText("Format"), { target: { value: "json" } });
    await waitFor(() =>
      expect(calls.previews.at(-1)?.options).toEqual({ format: "json", layout: "auto" }),
    );
  });

  it("shows why a preview failed", async () => {
    await renderWizard();
    previewFails = true;
    fireEvent.click(screen.getByRole("button", { name: "Choose file…" }));
    expect(await screen.findByTestId("preview-error")).toHaveTextContent(
      "Could not read the file: line 3: unterminated quote",
    );
    expect(screen.getByRole("button", { name: "Next" })).toBeDisabled();
  });

  it("validates the mapping of a new table", async () => {
    await renderWizard({ sessionId: "s-1", target: { kind: "schema", name: "public" } });
    await toMapping();
    // A new table: name suggested from the file, schema from the tree.
    fireEvent.change(screen.getByLabelText("New column for amount"), { target: { value: "id" } });
    next();
    const alert = await screen.findByTestId("mapping-problems");
    expect(alert).toHaveTextContent(
      '"id", "amount" all go to "id": each target column takes one source column.',
    );
    fireEvent.change(screen.getByLabelText("New column for amount"), { target: { value: "" } });
    expect(screen.getByTestId("mapping-problems")).toHaveTextContent(
      'Name the new column for "amount".',
    );
    for (const name of ["id", "amount", "note"]) {
      fireEvent.click(screen.getByLabelText(`Import ${name}`));
    }
    expect(screen.getByTestId("mapping-problems")).toHaveTextContent("Import at least one column.");
    expect(calls.plans).toHaveLength(0);
  });

  it("plans and runs a new-table import with progress, cancel and a report", async () => {
    const { onClose } = await renderWizard({
      sessionId: "s-1",
      target: { kind: "schema", catalog: "app", name: "public" },
    });
    await pickAndPreview();
    next();
    expect(await screen.findByLabelText("Table")).toHaveValue("orders_2026");
    expect(screen.getByLabelText("Schema")).toHaveValue("public");
    expect(screen.getByLabelText("Database")).toHaveValue("app");
    next();
    await screen.findByRole("table", { name: "Column mapping" });
    // Overrides: note as varchar(255), id the primary key, amount skipped.
    fireEvent.change(screen.getByLabelText("Type of note"), {
      target: { value: JSON.stringify({ type: "var_char", length: 255 }) },
    });
    fireEvent.click(screen.getByLabelText("Primary key: id"));
    fireEvent.click(screen.getByLabelText("Skip bad rows, up to"));
    next();

    await screen.findByTestId("plan-ddl");
    const request = calls.plans[0] as ImportRequest;
    expect(request.target).toEqual({
      session: "s-1",
      table: { kind: "table", catalog: "app", schema: "public", name: "orders_2026" },
      mode: "create",
    });
    expect(request.primaryKey).toEqual(["id"]);
    expect(request.mapping[2]).toEqual({
      source: 2,
      target: "note",
      dataType: { type: "var_char", length: 255 },
    });
    expect(request.columns[2]).toEqual({
      name: "note",
      dataType: { type: "var_char", length: 255 },
    });
    expect(request.errors).toEqual({ type: "skip", maxErrors: 100 });
    expect(screen.getByText("decimal text, exact")).toBeInTheDocument();
    expect(screen.getByText("COPY (text)")).toBeInTheDocument();

    fireEvent.click(screen.getByTestId("start-import"));
    await waitFor(() => expect(calls.runs).toHaveLength(1));
    send({ type: "started", importId: "imp-1", totalBytes: 2_000_000, plan: plan(request) });
    send({
      type: "progress",
      bytesRead: 1_000_000,
      totalBytes: 2_000_000,
      rowsRead: 10_000,
      rowsInserted: 9_000,
      rowsCommitted: 5_000,
      rowsSkipped: 2,
      elapsedMs: 2000,
    });
    send({
      type: "rowError",
      error: { row: 7, column: "amount", message: '"n/a" is not a decimal' },
    });
    expect(screen.getByTestId("progress-done")).toHaveTextContent("50%");
    expect(screen.getByTestId("progress-read")).toHaveTextContent("1.00 MB of 2.00 MB");
    expect(screen.getByTestId("progress-rows-read")).toHaveTextContent("10,000");
    expect(screen.getByTestId("progress-rows-inserted")).toHaveTextContent("9,000");
    expect(screen.getByTestId("progress-rows-committed")).toHaveTextContent("5,000");
    expect(screen.getByTestId("progress-errors")).toHaveTextContent("2");
    expect(screen.getByTestId("progress-rate")).toHaveTextContent("5,000 rows/s · 500 KB/s read");
    expect(screen.getByTestId("progress-time-left")).toHaveTextContent("about 2.00 s");
    expect(screen.getByRole("progressbar", { name: "Progress" })).toHaveAttribute("value", "50");
    expect(screen.getByRole("table", { name: "Error log" })).toHaveTextContent("not a decimal");

    // Escape does not close a running import.
    fireEvent.keyDown(screen.getByRole("dialog"), { key: "Escape" });
    expect(onClose).not.toHaveBeenCalled();

    fireEvent.click(screen.getByTestId("cancel-import"));
    await waitFor(() => expect(calls.cancels).toEqual(["imp-1"]));
    expect(screen.getByTestId("cancel-import")).toHaveTextContent("Cancelling…");
    send({ type: "cancelled", report: report() });

    const result = screen.getByTestId("import-report");
    expect(result).toHaveTextContent("Import cancelled");
    expect(result).toHaveTextContent("The import was rolled back: the new table is gone.");
    expect(screen.getByTestId("report-rows-read")).toHaveTextContent("10,000");
    expect(screen.getByTestId("report-rows-skipped")).toHaveTextContent("2");
    const log = screen.getByRole("table", { name: "Error log" });
    expect(within(log).getAllByRole("row")).toHaveLength(3);
    expect(log).toHaveTextContent("out of range");
    fireEvent.click(screen.getByRole("button", { name: "Close" }));
    expect(onClose).toHaveBeenCalled();
  });

  it("imports into an existing table: loads its columns, requires NOT NULL ones", async () => {
    await renderWizard({
      sessionId: "s-1",
      target: { kind: "table", catalog: "app", schema: "public", name: "orders" },
    });
    await pickAndPreview();
    next();
    expect(await screen.findByLabelText("Table")).toHaveValue("orders");
    expect(screen.getByRole("radio", { name: "Existing table" })).toBeChecked();
    fireEvent.change(screen.getByLabelText("Before importing"), { target: { value: "truncate" } });
    next();
    await screen.findByRole("table", { name: "Column mapping" });
    expect(calls.loads).toEqual([
      { kind: "table", catalog: "app", schema: "public", name: "orders" },
    ]);
    expect(screen.getByLabelText("Target column for amount")).toHaveValue("amount");
    next();
    expect(await screen.findByTestId("mapping-problems")).toHaveTextContent(
      '"created_at" is NOT NULL and has no default: map a source column to it.',
    );
    fireEvent.change(screen.getByLabelText("Target column for note"), {
      target: { value: "created_at" },
    });
    expect(screen.queryByTestId("mapping-problems")).toBeNull();

    planExtra = { destructive: true };
    next();
    expect(await screen.findByTestId("destructive-warning")).toBeInTheDocument();
    const request = calls.plans[0] as ImportRequest;
    expect(request.target.mode).toBe("truncate");
    expect(request.mapping).toEqual([
      { source: 0, target: "id" },
      { source: 1, target: "amount" },
      { source: 2, target: "created_at" },
    ]);
    expect(screen.getByTestId("start-import")).toBeDisabled();
    fireEvent.click(screen.getByLabelText("I understand; import anyway"));
    expect(screen.getByTestId("start-import")).toBeEnabled();

    fireEvent.click(screen.getByTestId("start-import"));
    await waitFor(() => expect(calls.runs).toHaveLength(1));
    send({
      type: "failed",
      error: { category: "sql", message: "duplicate key in rows 1-50000" },
      report: report({ rowsCommitted: 0, errors: [], leftBehind: "The table is empty." }),
    });
    const result = screen.getByTestId("import-report");
    expect(result).toHaveTextContent("Import failed");
    expect(result).toHaveTextContent("duplicate key in rows 1-50000");
    expect(result).toHaveTextContent("The table is empty.");
  });

  it("explains a table that cannot be loaded and releases the file on close", async () => {
    tauriCore.invoke.mockImplementation(async (cmd, args) => {
      const a = (args ?? {}) as Record<string, unknown>;
      if (cmd === "list_open_connections") return [SESSION];
      if (cmd === "file_pick_open") return PICKED;
      if (cmd === "import_preview") return PREVIEW;
      if (cmd === "cancel_request") return true;
      if (cmd === "file_release") {
        calls.released.push(a.handle as string);
        return true;
      }
      if (cmd === "object_load") throw { category: "not_found", message: "no such table" };
      throw new Error(`unexpected ${cmd}`);
    });
    const { unmount } = await renderWizard({
      sessionId: "s-1",
      target: { kind: "table", schema: "public", name: "gone" },
    });
    await pickAndPreview();
    next();
    await screen.findByLabelText("Table");
    next();
    expect(await screen.findByTestId("target-error")).toHaveTextContent(
      "Could not load public.gone: no such table",
    );
    unmount();
    expect(calls.released).toEqual(["h-1"]);
  });
  it("keeps only the latest plan: a late answer cannot skip the destructive confirmation", async () => {
    deferPlans = true;
    await renderWizard({
      sessionId: "s-1",
      target: { kind: "table", catalog: "app", schema: "public", name: "orders" },
    });
    await pickAndPreview();
    next();
    await screen.findByLabelText("Before importing");
    next();
    await screen.findByRole("table", { name: "Column mapping" });
    fireEvent.change(screen.getByLabelText("Target column for note"), {
      target: { value: "created_at" },
    });
    // Plan 1: append (slow, not destructive).
    next();
    await waitFor(() => expect(calls.plans).toHaveLength(1));
    expect(calls.plans[0]?.target.mode).toBe("append");
    const firstId = calls.planIds[0];
    expect(firstId).toBeTruthy();
    // Back twice, TRUNCATE, Next twice: plan 2.
    fireEvent.click(screen.getByRole("button", { name: "Back" }));
    expect(calls.cancelledRequests).toContain(firstId);
    fireEvent.click(screen.getByRole("button", { name: "Back" }));
    fireEvent.change(screen.getByLabelText("Before importing"), { target: { value: "truncate" } });
    next();
    await screen.findByRole("table", { name: "Column mapping" });
    next();
    await waitFor(() => expect(calls.plans).toHaveLength(2));
    expect(calls.plans[1]?.target.mode).toBe("truncate");
    // Plan 2 answers first, then plan 1.
    const [first, second] = pendingPlans;
    await act(async () => second?.resolve({ destructive: true }));
    await act(async () => first?.resolve({ destructive: false }));
    expect(screen.getByTestId("destructive-warning")).toBeInTheDocument();
    expect(screen.getByText(MODE_TEXT.truncate)).toBeInTheDocument();
    expect(screen.getByTestId("start-import")).toBeDisabled();
    fireEvent.click(screen.getByLabelText("I understand; import anyway"));
    fireEvent.click(screen.getByTestId("start-import"));
    await waitFor(() => expect(calls.runs).toHaveLength(1));
    // The run sends exactly the request that was planned and confirmed.
    expect(calls.runs[0]).toEqual(calls.plans[1]);
  });

  it("reads an existing varchar column as text, following the chosen target column", async () => {
    tauriCore.invoke.mockImplementation(
      wrapLoad({
        ...ORDERS,
        columns: ORDERS.columns.map((c) =>
          c.name === "id"
            ? {
                ...c,
                data_type: { type: "var_char" as const, length: 20 },
                native_type: "varchar(20)",
              }
            : c,
        ),
      }),
    );
    await renderWizard({
      sessionId: "s-1",
      target: { kind: "table", catalog: "app", schema: "public", name: "orders" },
    });
    await pickAndPreview();
    next();
    await screen.findByLabelText("Before importing");
    next();
    await screen.findByRole("table", { name: "Column mapping" });
    expect(screen.getByLabelText("Type of id")).toHaveValue(JSON.stringify({ type: "text" }));
    fireEvent.change(screen.getByLabelText("Target column for note"), {
      target: { value: "created_at" },
    });
    expect(screen.getByLabelText("Type of note")).toHaveValue(
      JSON.stringify({ type: "timestamp", precision: 6 }),
    );
    next();
    await waitFor(() => expect(calls.plans).toHaveLength(1));
    expect(calls.plans[0]?.columns.map((c) => c.dataType)).toEqual([
      { type: "text" },
      { type: "decimal", precision: 10, scale: 2 },
      { type: "timestamp", precision: 6 },
    ]);
  });

  it("sizes a new decimal or varchar column", async () => {
    await renderWizard({ sessionId: "s-1", target: { kind: "schema", name: "public" } });
    await toMapping();
    fireEvent.change(screen.getByLabelText("Scale of amount"), { target: { value: "12" } });
    fireEvent.change(screen.getByLabelText("Precision of amount"), { target: { value: "30" } });
    fireEvent.change(screen.getByLabelText("Type of note"), {
      target: { value: JSON.stringify({ type: "var_char", length: 255 }) },
    });
    fireEvent.change(screen.getByLabelText("Length of note"), { target: { value: "4000" } });
    expect(screen.getByLabelText("Type of note")).toHaveValue(
      JSON.stringify({ type: "var_char", length: 4000 }),
    );
    next();
    await waitFor(() => expect(calls.plans).toHaveLength(1));
    expect(calls.plans[0]?.mapping.map((m) => m.dataType)).toEqual([
      { type: "int", bits: 64, unsigned: false },
      { type: "decimal", precision: 30, scale: 12 },
      { type: "var_char", length: 4000 },
    ]);
  });

  it("says when the live error log stops at the errors the core sends", async () => {
    await renderWizard({ sessionId: "s-1", target: { kind: "schema", name: "public" } });
    await toMapping();
    next();
    await screen.findByTestId("plan-ddl");
    fireEvent.click(screen.getByTestId("start-import"));
    await waitFor(() => expect(calls.runs).toHaveLength(1));
    send({
      type: "started",
      importId: "imp-1",
      totalBytes: 2_000_000,
      plan: plan(calls.runs[0] as ImportRequest),
    });
    send({ type: "rowError", error: { row: 3, message: "bad" } });
    expect(screen.queryByText(/Only the first/)).toBeNull();
    send({
      type: "progress",
      bytesRead: 10,
      totalBytes: 2_000_000,
      rowsRead: 500,
      rowsInserted: 0,
      rowsCommitted: 0,
      rowsSkipped: 250,
      elapsedMs: 1000,
    });
    expect(screen.getByText("Only the first 1 errors are listed.")).toBeInTheDocument();
  });

  it("releases a file picked after the wizard was closed", async () => {
    let answer: (file: PickedFile | null) => void = () => {};
    pickGate = new Promise((resolve) => {
      answer = resolve;
    });
    const { unmount } = await renderWizard();
    fireEvent.click(screen.getByRole("button", { name: "Choose file…" }));
    unmount();
    await act(async () => answer({ ...PICKED, handle: "h-late" }));
    await waitFor(() => expect(calls.released).toEqual(["h-late"]));
  });
});
