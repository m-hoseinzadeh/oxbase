import { describe, expect, it } from "vitest";
import type { RunEvent, StatementSpan } from "../../api";
import { IDLE_RUN, type RunState, rowCountLabel, runReducer } from "./runState";

const SCRIPT = "SELECT * FROM big;\nUPDATE t SET x = 1;";

function span(index: number, startLine: number, startColumn: number, endColumn: number) {
  return {
    index,
    startByte: 0,
    endByte: 0,
    startLine,
    startColumn,
    endLine: startLine,
    endColumn,
    returnsRows: index === 0,
  } satisfies StatementSpan;
}

const STATEMENTS = [span(0, 1, 1, 18), span(1, 2, 1, 19)];

function play(events: RunEvent[]): RunState {
  let state = runReducer(IDLE_RUN, { type: "start", script: SCRIPT });
  for (const event of events) state = runReducer(state, { type: "event", event });
  return state;
}

describe("runReducer", () => {
  it("folds a two-statement run into results and messages", () => {
    const state = play([
      { type: "started", runId: "r1", statements: STATEMENTS },
      { type: "statementStarted", statementIndex: 0 },
      {
        type: "resultSet",
        resultIndex: 0,
        statementIndex: 0,
        statementRange: STATEMENTS[0] as StatementSpan,
        columns: [{ name: "id", dataType: "Int64", nullable: false }],
      },
      { type: "rows", resultIndex: 0, totalSoFar: 1000 },
      { type: "rows", resultIndex: 0, totalSoFar: 600_000 },
      { type: "resultDone", resultIndex: 0, totalRows: 1_000_000, truncated: false },
      { type: "statementDone", statementIndex: 0, elapsedMs: 900 },
      { type: "statementStarted", statementIndex: 1 },
      { type: "notice", statementIndex: 1, level: "info", message: "hello", code: "00000" },
      { type: "statementDone", statementIndex: 1, rowsAffected: 3, elapsedMs: 5 },
      { type: "finished", elapsedMs: 910 },
    ]);
    expect(state.status).toBe("finished");
    expect(state.runId).toBe("r1");
    expect(state.results).toHaveLength(1);
    expect(state.results[0]).toMatchObject({ rowCount: 1_000_000, done: true, truncated: false });
    expect(state.messages.map((m) => [m.kind, m.text])).toEqual([
      ["ok", "1,000,000 rows returned"],
      ["notice", "hello (00000)"],
      ["ok", "3 rows affected"],
      ["summary", "Finished: 2 statements"],
    ]);
    expect(state.messages[0]?.statement).toBe("SELECT * FROM big");
    expect(state.messages[0]?.elapsedMs).toBe(900);
  });

  it("keeps the largest row count when lossy rows events arrive out of step", () => {
    const state = play([
      { type: "started", runId: "r1", statements: STATEMENTS },
      {
        type: "resultSet",
        resultIndex: 0,
        statementIndex: 0,
        statementRange: STATEMENTS[0] as StatementSpan,
        columns: [],
      },
      { type: "rows", resultIndex: 0, totalSoFar: 5000 },
      { type: "rows", resultIndex: 0, totalSoFar: 4000 },
    ]);
    expect(state.results[0]?.rowCount).toBe(5000);
    expect(rowCountLabel(state.results[0] as never)).toBe("5,000 rows…");
  });

  it("marks open results truncated on cancel and records failures", () => {
    const cancelled = play([
      { type: "started", runId: "r1", statements: STATEMENTS },
      {
        type: "resultSet",
        resultIndex: 0,
        statementIndex: 0,
        statementRange: STATEMENTS[0] as StatementSpan,
        columns: [],
      },
      { type: "rows", resultIndex: 0, totalSoFar: 42 },
      { type: "cancelled", elapsedMs: 120 },
    ]);
    expect(cancelled.status).toBe("cancelled");
    expect(cancelled.results[0]).toMatchObject({ rowCount: 42, done: true, truncated: true });
    expect(rowCountLabel(cancelled.results[0] as never)).toBe("42 rows (truncated)");

    const failed = play([
      { type: "started", runId: "r2", statements: STATEMENTS },
      {
        type: "statementFailed",
        statementIndex: 1,
        error: { category: "sql", message: "relation t does not exist" },
        elapsedMs: 2,
      },
      { type: "failed", error: { category: "sql", message: "statement 2 failed" }, elapsedMs: 3 },
    ]);
    expect(failed.status).toBe("failed");
    expect(failed.messages.map((m) => m.kind)).toEqual(["error", "error"]);
    expect(failed.messages[0]?.statement).toBe("UPDATE t SET x = 1");
  });

  it("ignores events after the end and reports start failures", () => {
    const done = play([{ type: "finished", elapsedMs: 1 }]);
    const after = runReducer(done, {
      type: "event",
      event: { type: "rows", resultIndex: 0, totalSoFar: 1 },
    });
    expect(after).toBe(done);
    const failed = runReducer(runReducer(IDLE_RUN, { type: "start", script: "x" }), {
      type: "startFailed",
      error: { category: "invalid_input", message: "a run is already active" },
    });
    expect(failed.status).toBe("failed");
    expect(failed.messages[0]?.text).toBe("a run is already active");
  });
});
