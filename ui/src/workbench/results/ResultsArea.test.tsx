import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { tableFromArrays, tableToIPC } from "apache-arrow";
import { useEffect } from "react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import type { RunEvent, StatementSpan } from "../../api";
import { tauriCore } from "../../test/tauriMock";
import type { ResultGridProps } from "./ResultGrid";
import { ResultsArea } from "./ResultsArea";
import { IDLE_RUN, type RunState, runReducer } from "./runState";

vi.mock("@tauri-apps/api/core", async () => (await import("../../test/tauriMock")).tauriCore);

// The canvas grid cannot draw in jsdom: a stand-in shows its row count and asks for the first
// screen of rows, as the real grid does when it mounts.
vi.mock("./ResultGrid", () => ({
  ResultGrid: ({ pager, rowCount, columns }: ResultGridProps) => {
    useEffect(() => pager.setVisible(0, 39), [pager]);
    useEffect(() => pager.setRowCount(rowCount), [pager, rowCount]);
    return (
      <div data-testid="grid">
        {columns.map((c) => c.name).join(",")} / {rowCount}
      </div>
    );
  },
}));

const SPAN: StatementSpan = {
  index: 0,
  startByte: 0,
  endByte: 8,
  startLine: 1,
  startColumn: 1,
  endLine: 1,
  endColumn: 9,
  returnsRows: true,
};

function apply(state: RunState, events: RunEvent[]): RunState {
  return events.reduce((s, event) => runReducer(s, { type: "event", event }), state);
}

const resultSet = (resultIndex: number): RunEvent => ({
  type: "resultSet",
  resultIndex,
  statementIndex: resultIndex,
  statementRange: SPAN,
  columns: [{ name: `c${resultIndex}`, dataType: "Int64", nullable: false }],
});

describe("ResultsArea", () => {
  beforeEach(() => {
    tauriCore.invoke.mockReset();
    tauriCore.isTauri.mockReturnValue(true);
    tauriCore.invoke.mockImplementation(async (cmd, args) => {
      if (cmd !== "query_page") throw new Error(`unexpected ${cmd}`);
      const { offset, limit } = args as { offset: number; limit: number };
      const ids = Float64Array.from({ length: limit }, (_, i) => offset + i);
      return tableToIPC(tableFromArrays({ c0: ids }), "stream").slice().buffer;
    });
  });

  it("shows the row count growing while the result streams", async () => {
    let run = apply(runReducer(IDLE_RUN, { type: "start", script: "SELECT 1" }), [
      { type: "started", runId: "run-1", statements: [SPAN] },
      { type: "statementStarted", statementIndex: 0 },
      resultSet(0),
      { type: "rows", resultIndex: 0, totalSoFar: 1000 },
    ]);
    const { rerender } = render(<ResultsArea run={run} />);
    expect(screen.getByRole("tab", { name: /Result 1/ })).toHaveAttribute("aria-selected", "true");
    expect(screen.getByTestId("result-status")).toHaveTextContent("1,000 rows…");
    expect(screen.getByTestId("grid")).toHaveTextContent("c0 / 1000");

    run = apply(run, [{ type: "rows", resultIndex: 0, totalSoFar: 250_000 }]);
    rerender(<ResultsArea run={run} />);
    expect(screen.getByTestId("result-status")).toHaveTextContent("250,000 rows…");
    expect(screen.getByTestId("grid")).toHaveTextContent("c0 / 250000");

    run = apply(run, [
      { type: "resultDone", resultIndex: 0, totalRows: 1_000_000, truncated: false },
      { type: "statementDone", statementIndex: 0, elapsedMs: 1500 },
      { type: "finished", elapsedMs: 1510 },
    ]);
    rerender(<ResultsArea run={run} />);
    expect(screen.getByTestId("result-status")).toHaveTextContent("1,000,000 rows");
    expect(screen.getByTestId("result-status")).not.toHaveTextContent("…");

    // Only the first screen of the million rows was asked for.
    await waitFor(() => expect(tauriCore.invoke).toHaveBeenCalled());
    for (const [cmd, args] of tauriCore.invoke.mock.calls) {
      expect(cmd).toBe("query_page");
      const { runId, resultIndex, offset, limit } = args as Record<string, number | string>;
      expect(runId).toBe("run-1");
      expect(resultIndex).toBe(0);
      expect(Number(offset) + Number(limit)).toBeLessThanOrEqual(1000);
    }
    // Every page fetch is timed for the webview smoke test.
    await waitFor(() => expect(window.__oxbasePerf?.pages.length ?? 0).toBeGreaterThan(0));
    const entry = window.__oxbasePerf?.pages.at(-1);
    expect(entry).toMatchObject({ offset: 0 });
    expect(entry?.ipcMs).toBeGreaterThanOrEqual(0);
    expect(entry?.decodeMs).toBeGreaterThanOrEqual(0);
  });

  it("has one tab per result set and a messages tab", async () => {
    const run = apply(runReducer(IDLE_RUN, { type: "start", script: "CALL p()" }), [
      { type: "started", runId: "run-2", statements: [SPAN] },
      resultSet(0),
      { type: "resultDone", resultIndex: 0, totalRows: 2, truncated: false },
      resultSet(1),
      { type: "resultDone", resultIndex: 1, totalRows: 1, truncated: true },
      { type: "notice", level: "warning", message: "row limit reached" },
      {
        type: "statementFailed",
        statementIndex: 0,
        error: { category: "sql", message: "boom" },
        elapsedMs: 4,
      },
      { type: "failed", error: { category: "sql", message: "boom" }, elapsedMs: 5 },
    ]);
    render(<ResultsArea run={run} />);
    const tabs = screen.getAllByRole("tab");
    expect(tabs.map((t) => t.textContent)).toEqual([
      "Result 12 rows",
      "Result 21 row (truncated)",
      "Messages2",
    ]);
    fireEvent.click(screen.getByRole("tab", { name: /Result 2/ }));
    expect(screen.getByTestId("grid")).toHaveTextContent("c1 / 1");

    // Arrow keys move between tabs.
    fireEvent.keyDown(screen.getByRole("tablist"), { key: "ArrowRight" });
    expect(screen.getByRole("tab", { name: /Messages/ })).toHaveAttribute("aria-selected", "true");
    const messages = screen.getByRole("list", { name: "Messages" });
    expect(messages).toHaveTextContent("row limit reached");
    expect(messages).toHaveTextContent("boom");
    expect(messages).toHaveTextContent("4 ms");
  });

  it("shows the messages tab before anything ran", () => {
    render(<ResultsArea run={IDLE_RUN} />);
    expect(screen.getByRole("tab", { name: "Messages" })).toHaveAttribute("aria-selected", "true");
    expect(screen.getByText("Run a query to see its results here.")).toBeInTheDocument();
  });
});
