import { act, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import type {
  StructureApplyReport,
  StructureCompareResult,
  StructureScriptRequest,
  SyncScript,
} from "../../api";
import { refreshSessions, resetConnectionsStore } from "../../connections/store";
import {
  callsOf,
  M8_SRC,
  M8_TGT,
  mockCore,
  PG_SOURCE,
  PG_TARGET,
  requestOf,
} from "../../test/m8Fixtures";
import type { SqlDiffViewProps } from "../editor/SqlDiffView";
import type { SqlEditorProps } from "../editor/SqlEditor";
import { StructureSyncPanel } from "./StructureSyncPanel";

vi.mock("@tauri-apps/api/core", async () => (await import("../../test/tauriMock")).tauriCore);
vi.mock("../editor/SqlEditor", () => ({
  SqlEditor: ({ value, readOnly, testId }: SqlEditorProps) => (
    <pre data-testid={testId} data-readonly={readOnly ? "yes" : "no"}>
      {value}
    </pre>
  ),
}));
vi.mock("../editor/SqlDiffView", () => ({
  SqlDiffView: ({ original, modified, testId }: SqlDiffViewProps) => (
    <div data-testid={testId}>
      <pre data-testid="diff-original">{original}</pre>
      <pre data-testid="diff-modified">{modified}</pre>
    </div>
  ),
}));

function compareResult(id: string, inSync = false): StructureCompareResult {
  const items: StructureCompareResult["diff"]["items"] = inSync
    ? [
        { id: "table:customers", kind: "table", name: "customers", status: "same" },
        { id: "table:legacy_notes", kind: "table", name: "legacy_notes", status: "only_in_target" },
      ]
    : [
        {
          id: "table:customers",
          kind: "table",
          name: "customers",
          status: "different",
          sourceDdl: "CREATE TABLE m8_tgt.customers (id integer, email varchar(200));",
          targetDdl: "CREATE TABLE m8_tgt.customers (id integer, email varchar(100), fax text);",
          properties: [
            {
              object: { kind: "column", name: "email" },
              prop: "data_type",
              kind: "changed",
              old: { type: "text", value: "varchar(100)" },
              new: { type: "text", value: "varchar(200)" },
            },
          ],
        },
        { id: "table:legacy_notes", kind: "table", name: "legacy_notes", status: "only_in_target" },
        { id: "table:order_lines", kind: "table", name: "order_lines", status: "only_in_source" },
        { id: "table:products", kind: "table", name: "products", status: "same" },
        {
          id: "view:active_customers",
          kind: "view",
          name: "active_customers",
          status: "only_in_source",
        },
      ];
  return {
    compareId: id,
    diff: { engine: "postgres", source: M8_SRC, target: M8_TGT, items },
    counts: inSync
      ? { onlyInSource: 0, onlyInTarget: 1, different: 0, same: 1 }
      : { onlyInSource: 2, onlyInTarget: 1, different: 1, same: 1 },
    elapsedMs: 120,
  };
}

function script(selection: string[], destructive: boolean, transactionalDdl = true): SyncScript {
  return {
    engine: "postgres",
    sql: selection.map((s) => `-- ${s}`).join("\n"),
    items: selection,
    destructive,
    atomic: transactionalDdl,
    transactionalDdl,
    script: {
      steps: [
        {
          sql: "ALTER TABLE m8_tgt.customers DROP COLUMN fax",
          description: "Drop column m8_tgt.customers.fax",
          destructive,
          transactional: true,
          phase: "main",
          check: "none",
        },
        {
          sql: "CREATE TABLE m8_tgt.order_lines (id integer)",
          destructive: false,
          transactional: true,
          phase: "main",
          check: "none",
        },
      ],
    },
  };
}

let transactional = true;

function setup() {
  mockCore({
    structure_compare: () => compareResult("cmp-1"),
    structure_script: (a) => {
      const request = a.request as StructureScriptRequest;
      return script(request.selection ?? [], true, transactional);
    },
    structure_apply: (): StructureApplyReport => ({
      script: script(["table:customers"], true),
      outcome: { committed: true, atomic: true, steps: [], warnings: [] },
      recompare: compareResult("cmp-2", true),
      inSync: true,
    }),
  });
}

async function renderPanel() {
  await refreshSessions();
  render(
    <StructureSyncPanel
      params={{
        source: { sessionId: PG_SOURCE.id, container: M8_SRC },
        target: { sessionId: PG_TARGET.id, container: M8_TGT },
      }}
    />,
  );
  const compare = screen.getByTestId("structure-compare");
  await waitFor(() => expect(compare).toBeEnabled());
  return compare;
}

describe("StructureSyncPanel", () => {
  beforeEach(() => {
    resetConnectionsStore();
    transactional = true;
    setup();
  });

  it("picks both sides from the entry point and compares them", async () => {
    const compare = await renderPanel();
    expect(screen.getByLabelText("Source schema")).toHaveValue("m8_src");
    expect(screen.getByLabelText("Target schema")).toHaveValue("m8_tgt");
    fireEvent.click(compare);
    await screen.findByTestId("structure-items");
    expect(callsOf("structure_compare")[0]?.request).toMatchObject({
      sourceSession: PG_SOURCE.id,
      source: M8_SRC,
      targetSession: PG_TARGET.id,
      target: M8_TGT,
      options: { includeDrops: false, compareComments: true },
    });
    expect(screen.getByTestId("structure-counts")).toHaveTextContent("1 different");

    // Grouped by kind, identical items hidden, side by side.
    const items = screen.getAllByTestId("structure-item");
    expect(items.map((i) => i.getAttribute("data-item-id"))).toEqual([
      "table:customers",
      "table:legacy_notes",
      "table:order_lines",
      "view:active_customers",
    ]);
    const drop = items[1] as HTMLElement;
    expect(within(drop).getByRole("checkbox")).not.toBeChecked();
    expect(within(drop).getByText("Only in target")).toBeInTheDocument();
    expect(within(items[0] as HTMLElement).getByRole("checkbox")).toBeChecked();
    fireEvent.click(screen.getByLabelText("Show identical"));
    expect(screen.getAllByTestId("structure-item")).toHaveLength(5);
    expect(screen.getByLabelText("Sync table products")).toBeDisabled();
  });

  it("shows an object's DDL diff and properties", async () => {
    fireEvent.click(await renderPanel());
    await screen.findByTestId("structure-items");
    // The first differing item is shown.
    expect(screen.getByTestId("diff-original")).toHaveTextContent("varchar(100), fax text");
    expect(screen.getByTestId("diff-modified")).toHaveTextContent("varchar(200)");
    const props = screen.getByTestId("structure-properties");
    expect(within(props).getByText("data_type")).toBeInTheDocument();
    expect(within(props).getByText("varchar(200)")).toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: /order_lines/ }));
    expect(screen.getByTestId("structure-object")).toHaveTextContent("sync will create it");
  });

  it("builds the script from the selection and re-builds it when the selection changes", async () => {
    fireEvent.click(await renderPanel());
    await screen.findByTestId("structure-items");
    fireEvent.click(screen.getByTestId("structure-script-tab"));
    await waitFor(() => expect(callsOf("structure_script")).toHaveLength(1));
    expect(callsOf("structure_script")[0]?.request).toEqual({
      compareId: "cmp-1",
      selection: ["table:customers", "table:order_lines", "view:active_customers"],
    });
    expect(await screen.findByTestId("structure-script")).toHaveAttribute("data-readonly", "yes");
    expect(screen.getByTestId("structure-script")).toHaveTextContent("-- table:customers");
    fireEvent.click(screen.getByLabelText("Sync all views"));
    await waitFor(() => expect(callsOf("structure_script")).toHaveLength(2));
    expect(requestOf<StructureScriptRequest>("structure_script", 1).selection).toEqual([
      "table:customers",
      "table:order_lines",
    ]);
  });

  it("confirms destructive steps before applying, then shows the re-compare", async () => {
    fireEvent.click(await renderPanel());
    await screen.findByTestId("structure-items");
    const apply = screen.getByTestId("structure-apply");
    await waitFor(() => expect(apply).toBeEnabled());
    fireEvent.click(apply);
    const dialog = screen.getByRole("dialog", { name: "Apply the sync script?" });
    // The target is named with its connection.
    expect(within(dialog).getByTestId("structure-confirm-target")).toHaveTextContent(
      "PG Target · oxbase_test.m8_tgt",
    );
    expect(within(dialog).getByTestId("structure-destructive")).toHaveTextContent(
      "Drop column m8_tgt.customers.fax",
    );
    const confirm = within(dialog).getByTestId("structure-confirm");
    expect(confirm).toBeDisabled();
    expect(callsOf("structure_apply")).toHaveLength(0);
    fireEvent.click(within(dialog).getByTestId("structure-acknowledge"));
    await act(async () => fireEvent.click(confirm));
    await waitFor(() => expect(callsOf("structure_apply")).toHaveLength(1));
    expect(callsOf("structure_apply")[0]?.request).toEqual({
      compareId: "cmp-1",
      selection: ["table:customers", "table:order_lines", "view:active_customers"],
      confirmDestructive: true,
    });
    expect(await screen.findByTestId("structure-result")).toHaveTextContent("In sync");
    // The re-compare replaces the result; the old compare is forgotten.
    await waitFor(() =>
      expect(callsOf("compare_forget").map((a) => a.compareId)).toContain("cmp-1"),
    );
    expect(screen.getAllByTestId("structure-item")).toHaveLength(1);
  });

  it("leaves drops out of a group select-all and flags a compare whose inputs changed", async () => {
    fireEvent.click(await renderPanel());
    await screen.findByTestId("structure-items");
    const all = screen.getByLabelText("Sync all tables");
    fireEvent.click(all);
    expect(screen.getByLabelText("Sync table customers")).not.toBeChecked();
    fireEvent.click(all);
    expect(screen.getByLabelText("Sync table customers")).toBeChecked();
    // The compare had no drops: the group does not pick the target-only table.
    expect(screen.getByLabelText("Sync table legacy_notes")).not.toBeChecked();
    expect(all).toBeChecked();
    const apply = screen.getByTestId("structure-apply");
    await waitFor(() => expect(apply).toBeEnabled());

    // Ticking "drop" after the compare makes it out of date: compare again before applying.
    fireEvent.click(screen.getByLabelText("Drop objects that exist only in the target"));
    expect(screen.getByTestId("structure-stale")).toHaveTextContent(
      "target PG Target · oxbase_test.m8_tgt",
    );
    expect(apply).toBeDisabled();
    fireEvent.click(screen.getByLabelText("Drop objects that exist only in the target"));
    expect(screen.queryByTestId("structure-stale")).not.toBeInTheDocument();
    // So does pointing the target picker elsewhere.
    fireEvent.change(screen.getByLabelText("Target schema"), { target: { value: "m8_src" } });
    expect(await screen.findByTestId("structure-stale")).toBeInTheDocument();
    expect(apply).toBeDisabled();
  });

  it("cancels a running apply", async () => {
    let rejectApply: ((e: unknown) => void) | null = null;
    mockCore({
      structure_compare: () => compareResult("cmp-1"),
      structure_script: (a) =>
        script((a.request as StructureScriptRequest).selection ?? [], false, true),
      structure_apply: () =>
        new Promise((_, reject) => {
          rejectApply = reject;
        }),
      cancel_request: () => {
        rejectApply?.({ category: "cancelled", message: "cancelled" });
        return true;
      },
    });
    fireEvent.click(await renderPanel());
    await screen.findByTestId("structure-items");
    const apply = screen.getByTestId("structure-apply");
    await waitFor(() => expect(apply).toBeEnabled());
    fireEvent.click(apply);
    await act(async () => fireEvent.click(screen.getByTestId("structure-confirm")));
    await waitFor(() => expect(callsOf("structure_apply")).toHaveLength(1));
    const requestId = callsOf("structure_apply")[0]?.requestId;
    await act(async () => fireEvent.click(await screen.findByTestId("structure-apply-cancel")));
    expect(callsOf("cancel_request").map((a) => a.requestId)).toContain(requestId);
    expect(await screen.findByTestId("structure-result")).toHaveTextContent("Sync cancelled");
  });

  it("says when the engine's DDL is not transactional", async () => {
    transactional = false;
    fireEvent.click(await renderPanel());
    await screen.findByTestId("structure-items");
    fireEvent.click(screen.getByTestId("structure-script-tab"));
    expect(await screen.findByText("DDL is not transactional on this engine")).toBeInTheDocument();
    const apply = screen.getByTestId("structure-apply");
    await waitFor(() => expect(apply).toBeEnabled());
    fireEvent.click(apply);
    expect(within(screen.getByRole("dialog")).getByText("Not transactional")).toBeInTheDocument();
  });

  it("does not compare a container with itself and reports compare errors", async () => {
    mockCore({
      structure_compare: () => {
        throw { category: "unsupported", message: "No comparator for view on this engine" };
      },
    });
    await refreshSessions();
    render(
      <StructureSyncPanel
        params={{
          source: { sessionId: PG_SOURCE.id, container: M8_SRC },
          target: { sessionId: PG_SOURCE.id, container: M8_SRC },
        }}
      />,
    );
    await waitFor(() => expect(screen.getByLabelText("Target schema")).toHaveValue("m8_src"));
    expect(screen.getByTestId("structure-compare")).toBeDisabled();
    fireEvent.change(screen.getByLabelText("Target schema"), { target: { value: "m8_tgt" } });
    await waitFor(() => expect(screen.getByTestId("structure-compare")).toBeEnabled());
    fireEvent.click(screen.getByTestId("structure-compare"));
    expect(await screen.findByTestId("structure-error")).toHaveTextContent(
      "No comparator for view on this engine",
    );
  });
});
