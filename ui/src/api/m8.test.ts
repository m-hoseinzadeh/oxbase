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
  BACKUP_FILTERS,
  type BackupEvent,
  backupCancel,
  backupList,
  backupRestore,
  backupRun,
  backupVerify,
  compareForget,
  type DataCompareEvent,
  dataApply,
  dataCancel,
  dataCompare,
  dataScript,
  isTerminalM8Event,
  type StructureCompareRequest,
  structureApply,
  structureCompare,
  structureScript,
  type TransferEvent,
  type TransferRequest,
  transferCancel,
  transferPairReport,
  transferPlan,
  transferRun,
  worstFidelity,
} from "./m8";

const transfer: TransferRequest = {
  sourceSession: "s-pg",
  targetSession: "s-my",
  spec: {
    objects: [
      {
        type: "table",
        table: { kind: "table", catalog: "oxbase_test", schema: "m8_xfer", name: "m8_types" },
      },
      { type: "query", sql: "SELECT 1 AS one", targetName: "m8_query" },
    ],
    target: { kind: "database", name: "m8_xfer_target" },
    options: { mode: "replace", fkChecks: "disable" },
    overrides: [{ object: 0, column: "c_interval", dataType: { type: "text" } }],
  },
  confirmDestructive: true,
};

const compare: StructureCompareRequest = {
  sourceSession: "s1",
  source: { kind: "schema", catalog: "oxbase_test", name: "m8_src" },
  targetSession: "s1",
  target: { kind: "schema", catalog: "oxbase_test", name: "m8_tgt" },
  options: { includeDrops: true },
};

/** The channel passed to the last call of `cmd`, with the other arguments. */
function lastChannel<T>(cmd: string): { onEvent: { onmessage: (e: T) => void }; rest: unknown } {
  const call = [...core.invoke.mock.calls].reverse().find(([c]) => c === cmd);
  const { onEvent, ...rest } = (call?.[1] ?? {}) as { onEvent: { onmessage: (e: T) => void } };
  return { onEvent, rest };
}

describe("M8 helpers", () => {
  it("combines fidelities from best to worst", () => {
    expect(worstFidelity()).toBe("exact");
    expect(worstFidelity("widened", "exact")).toBe("widened");
    expect(worstFidelity("narrowed", "unsupported", "widened")).toBe("unsupported");
  });

  it("has a backup file filter without dots", () => {
    expect(BACKUP_FILTERS[0]?.extensions).toEqual(["oxbackup"]);
  });
});

describe("transfer wrappers", () => {
  beforeEach(() => {
    core.invoke.mockReset();
    core.isTauri.mockReturnValue(true);
  });

  it("plans, reports pairs, runs and cancels", async () => {
    core.invoke.mockResolvedValue({});
    await transferPlan(transfer, "r1");
    expect(core.invoke).toHaveBeenLastCalledWith("transfer_plan", {
      request: transfer,
      requestId: "r1",
    });
    await transferPairReport("s-pg", "s-my");
    expect(core.invoke).toHaveBeenLastCalledWith("transfer_pair_report", {
      sourceSession: "s-pg",
      targetSession: "s-my",
    });

    core.invoke.mockResolvedValue("t-1");
    const seen: TransferEvent[] = [];
    expect(await transferRun(transfer, (e) => seen.push(e))).toBe("t-1");
    const { onEvent, rest } = lastChannel<TransferEvent>("transfer_run");
    expect(rest).toEqual({ request: transfer });
    onEvent.onmessage({ type: "tableStarted", table: 0 });
    expect(seen).toEqual([{ type: "tableStarted", table: 0 }]);
    expect(isTerminalM8Event(seen[0] as TransferEvent)).toBe(false);

    core.invoke.mockResolvedValue(true);
    await transferCancel("t-1");
    expect(core.invoke).toHaveBeenLastCalledWith("transfer_cancel", { transferId: "t-1" });
  });
});

describe("sync wrappers", () => {
  beforeEach(() => {
    core.invoke.mockReset();
    core.isTauri.mockReturnValue(true);
  });

  it("compares, scripts, applies and forgets structure", async () => {
    core.invoke.mockResolvedValue({});
    await structureCompare(compare);
    expect(core.invoke).toHaveBeenLastCalledWith("structure_compare", {
      request: compare,
      requestId: null,
    });
    await structureScript({ compareId: "c1", selection: ["table:orders"] });
    expect(core.invoke).toHaveBeenLastCalledWith("structure_script", {
      request: { compareId: "c1", selection: ["table:orders"] },
    });
    await structureApply({ compareId: "c1", confirmDestructive: true }, "r2");
    expect(core.invoke).toHaveBeenLastCalledWith("structure_apply", {
      request: { compareId: "c1", confirmDestructive: true },
      requestId: "r2",
    });
    core.invoke.mockResolvedValue(true);
    expect(await compareForget("c1")).toBe(true);
    expect(core.invoke).toHaveBeenLastCalledWith("compare_forget", { compareId: "c1" });
  });

  it("compares data with a channel, previews and applies", async () => {
    core.invoke.mockResolvedValue("d-1");
    const seen: DataCompareEvent[] = [];
    const request = {
      sourceSession: "s1",
      targetSession: "s1",
      pairs: [
        {
          source: { kind: "table" as const, schema: "m8_src", name: "m8_big" },
          target: { kind: "table" as const, schema: "m8_tgt", name: "m8_big" },
          key: { type: "primary" as const },
        },
      ],
      options: { chunkRows: 5000 },
    };
    expect(await dataCompare(request, (e) => seen.push(e))).toBe("d-1");
    const { onEvent, rest } = lastChannel<DataCompareEvent>("data_compare");
    expect(rest).toEqual({ request });
    onEvent.onmessage({
      type: "cancelled",
      result: { compareId: "d-1", tables: [], elapsedMs: 1 },
    });
    expect(isTerminalM8Event(seen[0] as DataCompareEvent)).toBe(true);

    core.invoke.mockResolvedValue({});
    await dataScript({ compareId: "d-1", options: { deletes: true } });
    expect(core.invoke).toHaveBeenLastCalledWith("data_script", {
      request: { compareId: "d-1", options: { deletes: true } },
      requestId: null,
    });
    core.invoke.mockResolvedValue("a-1");
    await dataApply({ compareId: "d-1", options: { confirmDeletes: true } }, () => {});
    expect(lastChannel("data_apply").rest).toEqual({
      request: { compareId: "d-1", options: { confirmDeletes: true } },
    });
    core.invoke.mockResolvedValue(false);
    expect(await dataCancel("a-1")).toBe(false);
    expect(core.invoke).toHaveBeenLastCalledWith("data_cancel", { operationId: "a-1" });
  });
});

describe("backup wrappers", () => {
  beforeEach(() => {
    core.invoke.mockReset();
    core.isTauri.mockReturnValue(true);
  });

  it("uses file handles and never paths", async () => {
    core.invoke.mockResolvedValue("b-1");
    const seen: BackupEvent[] = [];
    const request = {
      session: "s1",
      spec: { container: { kind: "database" as const, name: "m8_src" } },
    };
    expect(await backupRun("h-save", request, (e) => seen.push(e))).toBe("b-1");
    const { onEvent, rest } = lastChannel<BackupEvent>("backup_run");
    expect(rest).toEqual({ file: "h-save", request });
    onEvent.onmessage({ type: "cancelled", elapsedMs: 2 });
    expect(seen).toHaveLength(1);

    core.invoke.mockResolvedValue("r-1");
    const restore = {
      session: "s2",
      options: { container: { kind: "database" as const, name: "m8_restore" }, dropExisting: true },
    };
    await backupRestore("h-open", restore, () => {});
    expect(lastChannel("backup_restore").rest).toEqual({ file: "h-open", request: restore });

    core.invoke.mockResolvedValue({});
    await backupVerify("h-open");
    expect(core.invoke).toHaveBeenLastCalledWith("backup_verify", {
      file: "h-open",
      requestId: null,
    });
    await backupList("h-open", "r9");
    expect(core.invoke).toHaveBeenLastCalledWith("backup_list", {
      file: "h-open",
      requestId: "r9",
    });
    core.invoke.mockResolvedValue(true);
    await backupCancel("b-1");
    expect(core.invoke).toHaveBeenLastCalledWith("backup_cancel", { operationId: "b-1" });
  });
});
