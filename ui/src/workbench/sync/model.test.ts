import { describe, expect, it } from "vitest";
import type {
  DataCompareResult,
  DiffItem,
  SyncScript,
  TableDataDiff,
  TablePlan,
  TransferPlan,
} from "../../api";
import {
  autoPairs,
  backupFileName,
  backupReducer,
  buildSpec,
  compareStatusText,
  containerLabel,
  dataApplyReducer,
  dataCompareReducer,
  dataTotals,
  defaultSelection,
  destructiveSteps,
  groupItems,
  groupState,
  IDLE_BACKUP,
  IDLE_DATA_APPLY,
  IDLE_DATA_COMPARE,
  IDLE_RESTORE,
  IDLE_TRANSFER,
  itemKey,
  itemsProblem,
  orderedSelection,
  overrideKey,
  overridesOf,
  parseKeyColumns,
  planBlockers,
  planDdl,
  refKey,
  restoreReducer,
  setGroup,
  toggle,
  transferReducer,
  transferTotals,
  valueText,
} from "./model";

const ITEMS: DiffItem[] = [
  { id: "table:customers", kind: "table", name: "customers", status: "different" },
  { id: "table:legacy_notes", kind: "table", name: "legacy_notes", status: "only_in_target" },
  { id: "table:order_lines", kind: "table", name: "order_lines", status: "only_in_source" },
  { id: "table:same", kind: "table", name: "same", status: "same" },
  { id: "view:active_customers", kind: "view", name: "active_customers", status: "only_in_source" },
  {
    id: "trigger:orders.orders_audit",
    kind: "trigger",
    name: "orders_audit",
    parent: "orders",
    status: "different",
  },
];

describe("structure sync model", () => {
  it("groups items by kind in the core's order and hides identical ones", () => {
    const groups = groupItems(ITEMS, false);
    expect(groups.map((g) => [g.label, g.items.length])).toEqual([
      ["Tables", 3],
      ["Views", 1],
      ["Triggers", 1],
    ]);
    expect(groupItems(ITEMS, true)[0]?.items).toHaveLength(4);
  });

  it("selects creates and alters by default, drops only with includeDrops", () => {
    expect([...defaultSelection(ITEMS, false)].sort()).toEqual([
      "table:customers",
      "table:order_lines",
      "trigger:orders.orders_audit",
      "view:active_customers",
    ]);
    expect(defaultSelection(ITEMS, true).has("table:legacy_notes")).toBe(true);
    expect(defaultSelection(ITEMS, true).has("table:same")).toBe(false);
  });

  it("tracks group checkbox states and keeps the selection in item order", () => {
    const tables = groupItems(ITEMS, true)[0];
    if (!tables) throw new Error("no tables");
    let selection = defaultSelection(ITEMS, false);
    expect(groupState(tables, selection)).toBe("mixed");
    selection = setGroup(selection, tables, true);
    expect(groupState(tables, selection)).toBe(true);
    // Identical items are never selected.
    expect(selection.has("table:same")).toBe(false);
    selection = setGroup(selection, tables, false);
    expect(groupState(tables, selection)).toBe(false);
    // Without drops in the compare, the group leaves target-only objects alone.
    let noDrops = setGroup(defaultSelection(ITEMS, false), tables, true, false);
    expect(noDrops.has("table:legacy_notes")).toBe(false);
    expect(groupState(tables, noDrops, false)).toBe(true);
    noDrops = toggle(noDrops, "table:legacy_notes");
    noDrops = setGroup(noDrops, tables, false, false);
    expect(noDrops.has("table:legacy_notes")).toBe(false);
    expect(groupState(tables, toggle(noDrops, "table:legacy_notes"), false)).toBe("mixed");
    selection = toggle(selection, "table:customers");
    expect(orderedSelection(ITEMS, selection)).toEqual([
      "table:customers",
      "view:active_customers",
      "trigger:orders.orders_audit",
    ]);
  });

  it("lists destructive steps by description", () => {
    const script: SyncScript = {
      engine: "postgres",
      sql: "",
      items: [],
      destructive: true,
      atomic: true,
      transactionalDdl: true,
      script: {
        steps: [
          {
            sql: "DROP TABLE legacy_notes",
            description: "Drop table m8_tgt.legacy_notes",
            destructive: true,
            transactional: true,
            phase: "main",
            check: "none",
          },
          {
            sql: "ALTER TABLE customers DROP COLUMN fax",
            destructive: true,
            transactional: true,
            phase: "main",
            check: "none",
          },
          {
            sql: "CREATE VIEW v AS SELECT 1",
            destructive: false,
            transactional: true,
            phase: "main",
            check: "none",
          },
        ],
      },
    };
    expect(destructiveSteps(script)).toEqual([
      "Drop table m8_tgt.legacy_notes",
      "ALTER TABLE customers DROP COLUMN fax",
    ]);
  });

  it("renders property values as text", () => {
    expect(valueText({ type: "text", value: "varchar(40)" })).toBe("varchar(40)");
    expect(valueText({ type: "bool", value: false })).toBe("false");
    expect(valueText({ type: "null" })).toBe("NULL");
    expect(valueText(undefined)).toBe("");
  });

  it("labels containers with the database first", () => {
    expect(containerLabel({ kind: "schema", catalog: "oxbase_test", name: "m8_src" })).toBe(
      "oxbase_test.m8_src",
    );
    expect(containerLabel({ kind: "database", name: "m8_src" })).toBe("m8_src");
    expect(containerLabel(null)).toBe("");
  });
});

const SRC = { kind: "table" as const, catalog: "oxbase_test", schema: "m8_xfer", name: "m8_types" };

function tablePlan(object: number, name: string, patch: Partial<TablePlan> = {}): TablePlan {
  return {
    object,
    target: { kind: "table", catalog: "m8_xfer_target", name },
    exists: false,
    mode: "create",
    columns: [],
    fidelity: "exact",
    ddl: [`CREATE TABLE ${name} (id int)`],
    ...patch,
  };
}

function plan(patch: Partial<TransferPlan> = {}): TransferPlan {
  return {
    sourceEngine: "postgres",
    targetEngine: "mysql",
    tables: [
      tablePlan(0, "m8_child", { foreignKeys: ["ALTER TABLE m8_child ADD FOREIGN KEY …"] }),
      tablePlan(1, "m8_parent", { estimatedRows: 10 }),
    ],
    order: [1, 0],
    fidelity: "narrowed",
    destructive: false,
    transactionalDdl: false,
    runnable: true,
    ...patch,
  };
}

describe("transfer model", () => {
  it("builds the spec: target names only when renamed, filters as raw SQL, overrides", () => {
    const parent = { ...SRC, name: "m8_parent" };
    const spec = buildSpec(
      [
        { ref: SRC, targetName: "m8_types", where: "" },
        { ref: parent, targetName: "parents", where: " id > 1 " },
        { ref: { ...SRC, kind: "view", name: "v" }, targetName: "v", where: "" },
      ],
      { kind: "database", name: "m8_xfer_target" },
      { mode: "append" },
      {
        [overrideKey(refKey(SRC), "c_uuid")]: { nativeType: " binary(16) " },
        [overrideKey(refKey(SRC), "c_interval")]: { skip: true },
        [overrideKey(refKey(parent), "nothing")]: { targetName: "  " },
      },
    );
    expect(spec.objects).toEqual([
      { type: "table", table: SRC },
      {
        type: "table",
        table: { ...SRC, name: "m8_parent" },
        targetName: "parents",
        filter: { type: "rawSql", sql: "id > 1" },
      },
      { type: "view", view: { ...SRC, kind: "view", name: "v" } },
    ]);
    expect(spec.overrides).toEqual([
      { object: 0, column: "c_interval", skip: true },
      { object: 0, column: "c_uuid", nativeType: "binary(16)" },
    ]);
    expect(overridesOf({}, [])).toEqual([]);
    expect(
      buildSpec([{ sql: "SELECT 1", targetName: "q", where: "" }], SRC, {}, {}).objects,
    ).toEqual([{ type: "query", sql: "SELECT 1", targetName: "q" }]);
  });

  it("keeps overrides with their source object when the list changes", () => {
    const parent = { ...SRC, name: "m8_parent" };
    const gone = { ...SRC, name: "m8_gone" };
    const overrides = {
      [overrideKey(refKey(parent), "name")]: { skip: true },
      [overrideKey(refKey(gone), "id")]: { skip: true },
      [overrideKey("query", "x")]: { nativeType: "text" },
    };
    // m8_parent was second; with the first object removed it is the first.
    expect(overridesOf(overrides, [{ ref: parent, targetName: "m8_parent", where: "" }])).toEqual([
      { object: 0, column: "name", skip: true },
    ]);
    expect(
      overridesOf(overrides, [
        { ref: SRC, targetName: "m8_types", where: "" },
        { ref: parent, targetName: "m8_parent", where: "" },
      ]),
    ).toEqual([{ object: 1, column: "name", skip: true }]);
    expect(itemKey({ sql: "SELECT 1", targetName: "q", where: "" })).toBe("query");
    // The same schema and name in another catalog is another object.
    expect(refKey({ ...SRC, catalog: "other" })).not.toBe(refKey(SRC));
  });

  it("finds problems in the object list", () => {
    expect(itemsProblem([])).toMatch(/at least one/);
    expect(itemsProblem([{ ref: SRC, targetName: " ", where: "" }])).toMatch(/target name/);
    expect(
      itemsProblem([
        { ref: SRC, targetName: "T", where: "" },
        { ref: { ...SRC, name: "x" }, targetName: "t", where: "" },
      ]),
    ).toMatch(/same target table "t"/);
    expect(itemsProblem([{ sql: " ", targetName: "q", where: "" }])).toMatch(/query is empty/);
    expect(itemsProblem([{ ref: SRC, targetName: "t", where: "" }])).toBeNull();
  });

  it("writes the dry-run DDL in load order with the foreign keys last", () => {
    const ddl = planDdl(plan());
    expect(ddl.indexOf("CREATE TABLE m8_parent")).toBeLessThan(
      ddl.indexOf("CREATE TABLE m8_child"),
    );
    expect(ddl.trimEnd().endsWith("ALTER TABLE m8_child ADD FOREIGN KEY …")).toBe(true);
    expect(planBlockers(plan({ runnable: false }))).toEqual(["The plan cannot run as it is."]);
    expect(planBlockers(plan({ runnable: false, blocked: ["c_x: unsupported"] }))).toEqual([
      "c_x: unsupported",
    ]);
  });

  it("follows the run's events to one terminal state", () => {
    let s = transferReducer(IDLE_TRANSFER, { type: "start" });
    s = transferReducer(s, {
      type: "event",
      event: { type: "started", transferId: "t-1", plan: plan() },
    });
    expect(s.status).toBe("running");
    s = transferReducer(s, { type: "event", event: { type: "tableStarted", table: 1 } });
    s = transferReducer(s, {
      type: "event",
      event: {
        type: "progress",
        progress: {
          table: 1,
          rowsRead: 5,
          rowsWritten: 4,
          rowsCommitted: 0,
          rowsSkipped: 0,
          bytes: 100,
          errors: 0,
          totalRows: 10,
          elapsedMs: 10,
        },
      },
    });
    s = transferReducer(s, {
      type: "event",
      event: { type: "rowError", table: 1, issue: { row: 3, column: "c", message: "too long" } },
    });
    expect(s.current).toBe(1);
    expect(s.issues).toHaveLength(1);
    const totals = transferTotals(s);
    expect(totals.rowsWritten).toBe(4);
    // Table 0 has no total yet: the overall total is unknown.
    expect(totals.rowsTotal).toBeNull();
    const done = {
      table: 1,
      target: { kind: "table" as const, name: "m8_parent" },
      status: "done" as const,
      progress: {
        table: 1,
        rowsRead: 10,
        rowsWritten: 10,
        rowsCommitted: 10,
        rowsSkipped: 0,
        bytes: 200,
        errors: 0,
        elapsedMs: 20,
      },
      issuesTruncated: false,
      leftBehind: "",
    };
    s = transferReducer(s, { type: "event", event: { type: "tableFinished", report: done } });
    expect(s.current).toBeNull();
    s = transferReducer(s, {
      type: "event",
      event: { type: "finished", report: { tables: [done], foreignKeysAdded: 1, elapsedMs: 30 } },
    });
    expect(s.status).toBe("finished");
    expect(s.report?.foreignKeysAdded).toBe(1);
    // Nothing after the terminal event changes the state.
    const after = transferReducer(s, { type: "event", event: { type: "tableStarted", table: 0 } });
    expect(after).toBe(s);
  });
});

function tableDiff(pair: number, name: string, counts: [number, number, number]): TableDataDiff {
  return {
    pair,
    source: { kind: "table", schema: "m8_src", name },
    target: { kind: "table", schema: "m8_tgt", name },
    status: { type: "compared" },
    key: ["id"],
    columns: ["id", "name"],
    counts: {
      sourceRows: 10,
      targetRows: 10,
      same: 1,
      inserts: counts[0],
      updates: counts[1],
      deletes: counts[2],
    },
    chunksSkipped: 0,
    elapsedMs: 5,
  };
}

describe("data sync model", () => {
  it("pairs tables by name, exact first, then ignoring case", () => {
    const s = (name: string) => ({ kind: "table" as const, schema: "a", name });
    const t = (name: string) => ({ kind: "table" as const, schema: "b", name });
    const pairs = autoPairs(
      [s("m8_data"), s("MixedCase"), s("only_src")],
      [t("mixedcase"), t("m8_data")],
    );
    expect(
      pairs.map((p) => [p.source.name, p.target?.name ?? null, p.include, p.caseFolded ?? false]),
    ).toEqual([
      ["m8_data", "m8_data", true, false],
      // Found only by ignoring case: offered, but not ticked.
      ["MixedCase", "mixedcase", false, true],
      ["only_src", null, false, false],
    ]);
    expect(parseKeyColumns(" a, b ,,")).toEqual(["a", "b"]);
    expect(parseKeyColumns("  ")).toBeNull();
  });

  it("collects compare results and adopts a re-compare", () => {
    let s = dataCompareReducer(IDLE_DATA_COMPARE, { type: "start" });
    s = dataCompareReducer(s, { type: "started", id: "op-1" });
    s = dataCompareReducer(s, {
      type: "event",
      event: { type: "started", compareId: "cmp-1", tables: 2 },
    });
    s = dataCompareReducer(s, {
      type: "event",
      event: { type: "tableFinished", table: tableDiff(0, "m8_data", [2, 5, 2]) },
    });
    const refused: TableDataDiff = {
      ...tableDiff(1, "m8_nokey", [0, 0, 0]),
      status: { type: "refused", reason: "No primary or unique key: choose key columns." },
    };
    const result: DataCompareResult = {
      compareId: "cmp-1",
      tables: [tableDiff(0, "m8_data", [2, 5, 2]), refused],
      elapsedMs: 40,
    };
    s = dataCompareReducer(s, { type: "event", event: { type: "finished", result } });
    expect(s.status).toBe("finished");
    expect(s.compareId).toBe("cmp-1");
    expect(dataTotals(Object.values(s.results))).toEqual({ inserts: 2, updates: 5, deletes: 2 });
    expect(compareStatusText(refused)).toMatch(/^Refused: No primary/);
    expect(compareStatusText(tableDiff(0, "x", [0, 0, 0]))).toBe("In sync");

    const again = dataCompareReducer(s, {
      type: "adopt",
      result: { compareId: "cmp-2", tables: [tableDiff(0, "m8_data", [0, 0, 0])], elapsedMs: 3 },
    });
    expect(again.compareId).toBe("cmp-2");
    expect(dataTotals(Object.values(again.results))).toEqual({
      inserts: 0,
      updates: 0,
      deletes: 0,
    });
  });

  it("follows an apply through its re-compare", () => {
    let s = dataApplyReducer(IDLE_DATA_APPLY, { type: "start" });
    s = dataApplyReducer(s, {
      type: "event",
      event: { type: "started", applyId: "a-1", totalStatements: 7 },
    });
    s = dataApplyReducer(s, {
      type: "event",
      event: { type: "progress", statementsDone: 3, totalStatements: 7, pair: 0, elapsedMs: 4 },
    });
    expect([s.done, s.total]).toEqual([3, 7]);
    s = dataApplyReducer(s, { type: "event", event: { type: "recomparing" } });
    expect(s.status).toBe("recomparing");
    s = dataApplyReducer(s, {
      type: "event",
      event: {
        type: "finished",
        report: {
          tables: [],
          statements: 7,
          committedBatches: 1,
          leftBehind: "",
          inSync: true,
          elapsedMs: 9,
        },
      },
    });
    expect(s.status).toBe("finished");
    expect(s.report?.inSync).toBe(true);
    expect(dataApplyReducer(s, { type: "reset" })).toBe(IDLE_DATA_APPLY);
  });
});

describe("backup model", () => {
  it("follows backup and restore runs", () => {
    let b = backupReducer(IDLE_BACKUP, { type: "start" });
    b = backupReducer(b, {
      type: "event",
      event: { type: "started", backupId: "b-1", objects: 4 },
    });
    b = backupReducer(b, {
      type: "event",
      event: {
        type: "progress",
        progress: { objectsDone: 2, objectsTotal: 4, rows: 10, bytes: 400, elapsedMs: 50 },
      },
    });
    expect(b.progress?.objectsDone).toBe(2);
    b = backupReducer(b, { type: "event", event: { type: "cancelled", elapsedMs: 60 } });
    expect([b.status, b.elapsedMs]).toEqual(["cancelled", 60]);

    let r = restoreReducer(IDLE_RESTORE, { type: "start" });
    r = restoreReducer(r, {
      type: "event",
      event: {
        type: "failed",
        error: { category: "sql", message: "boom" },
        report: { objects: [], rows: 0, leftBehind: "table x was created", elapsedMs: 1 },
      },
    });
    expect(r.status).toBe("failed");
    expect(r.report?.leftBehind).toBe("table x was created");
  });

  it("suggests a file name from the container", () => {
    const name = backupFileName(
      { kind: "schema", catalog: "oxbase_test", name: "m8_src" },
      new Date(2026, 10, 3, 9, 5),
    );
    expect(name).toBe("oxbase_test-m8_src-20261103-0905.oxbackup");
    expect(backupFileName(null, new Date(2026, 0, 1, 0, 0))).toBe("backup-20260101-0000.oxbackup");
  });
});
