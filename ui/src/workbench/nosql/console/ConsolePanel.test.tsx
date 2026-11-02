import { act, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { Field, Schema, Table, tableToIPC, Utf8, vectorFromArray } from "apache-arrow";
import { beforeEach, describe, expect, it, vi } from "vitest";
import type { ColumnInfo, RunEvent, StatementSpan } from "../../../api";
import { tauriCore } from "../../../test/tauriMock";
import { ConsolePanel, confirmationText } from "./ConsolePanel";

vi.mock("@tauri-apps/api/core", async () => (await import("../../../test/tauriMock")).tauriCore);

function span(text: string): StatementSpan {
  return {
    index: 0,
    startByte: 0,
    endByte: text.length,
    startLine: 1,
    startColumn: 1,
    endLine: 1,
    endColumn: text.length + 1,
    returnsRows: true,
  };
}

const GENERIC: ColumnInfo[] = ["#", "type", "value", "raw"].map((name) => ({
  name,
  dataType: "Utf8",
  nullable: true,
}));

function arrayPage(): Uint8Array {
  const names = ["#", "type", "value", "raw"];
  const table = new Table(new Schema(names.map((n) => new Field(n, new Utf8(), true))), {
    "#": vectorFromArray(["1", "2"], new Utf8()),
    type: vectorFromArray(["string", "string"], new Utf8()),
    value: vectorFromArray(["name", "Ada"], new Utf8()),
    raw: vectorFromArray([null, null], new Utf8()),
  });
  return tableToIPC(table, "stream");
}

let onEvent: ((event: RunEvent) => void) | null = null;
let refuse: string | null = null;

function mockCore() {
  tauriCore.invoke.mockImplementation(async (cmd, args) => {
    const a = (args ?? {}) as Record<string, unknown>;
    switch (cmd) {
      case "list_open_connections":
        return [];
      case "query_run": {
        const options = a.options as { confirmDestructive?: boolean } | null;
        if (refuse && !options?.confirmDestructive) {
          throw { category: "invalid_input", message: refuse };
        }
        onEvent = (a.onEvent as { onmessage: (e: RunEvent) => void }).onmessage;
        return "run-1";
      }
      case "query_page":
        return arrayPage();
      case "query_close_editor":
      case "query_cancel":
        return null;
      default:
        throw new Error(`unexpected command ${cmd}`);
    }
  });
}

function calls(cmd: string) {
  return tauriCore.invoke.mock.calls
    .filter(([c]) => c === cmd)
    .map(([, args]) => args as Record<string, unknown>);
}

function finishWithArray(text: string) {
  act(() => {
    onEvent?.({ type: "started", runId: "run-1", statements: [span(text)] });
    onEvent?.({ type: "statementStarted", statementIndex: 0 });
    onEvent?.({
      type: "resultSet",
      resultIndex: 0,
      statementIndex: 0,
      statementRange: span(text),
      columns: GENERIC,
    });
    onEvent?.({ type: "resultDone", resultIndex: 0, totalRows: 2, truncated: false });
    onEvent?.({ type: "statementDone", statementIndex: 0, elapsedMs: 1 });
    onEvent?.({ type: "finished", elapsedMs: 1 });
  });
}

function run(text: string) {
  const input = screen.getByTestId("console-input");
  fireEvent.change(input, { target: { value: text } });
  fireEvent.keyDown(input, { key: "Enter" });
}

describe("ConsolePanel", () => {
  beforeEach(() => {
    tauriCore.invoke.mockReset();
    tauriCore.isTauri.mockReturnValue(true);
    refuse = null;
    mockCore();
  });

  it("runs a command on its own editor session and shows a RESP-typed reply", async () => {
    render(<ConsolePanel editorId="con-1" sessionId="s-1" database="1" />);
    run("HGETALL m7:hash");
    await waitFor(() => expect(calls("query_run")).toHaveLength(1));
    expect(calls("query_run")[0]).toMatchObject({
      sessionId: "s-1",
      editorId: "con-1",
      script: "HGETALL m7:hash",
      options: { database: "1" },
    });
    finishWithArray("HGETALL m7:hash");
    const lines = await screen.findByTestId("resp-lines");
    expect(lines.textContent).toBe('1)"name"2)"Ada"');
    expect(screen.getByTestId("console-output")).toHaveTextContent("db1> HGETALL m7:hash");
  });

  it("shows rows affected as an integer reply and errors as (error)", async () => {
    render(<ConsolePanel editorId="con-1" sessionId="s-1" />);
    run("DEL a b");
    await waitFor(() => expect(calls("query_run")).toHaveLength(1));
    act(() => {
      onEvent?.({ type: "started", runId: "run-1", statements: [span("DEL a b")] });
      onEvent?.({ type: "statementDone", statementIndex: 0, rowsAffected: 2, elapsedMs: 1 });
      onEvent?.({ type: "finished", elapsedMs: 1 });
    });
    expect(await screen.findByText("(integer) 2")).toBeInTheDocument();
    run("GET");
    await waitFor(() => expect(calls("query_run")).toHaveLength(2));
    act(() => {
      onEvent?.({ type: "started", runId: "run-1", statements: [span("GET")] });
      onEvent?.({
        type: "statementFailed",
        statementIndex: 0,
        error: { category: "sql", message: "ERR wrong number of arguments" },
        elapsedMs: 1,
      });
      onEvent?.({
        type: "failed",
        error: { category: "sql", message: "ERR wrong number of arguments" },
        elapsedMs: 1,
      });
    });
    expect(await screen.findByText("(error) ERR wrong number of arguments")).toBeInTheDocument();
  });

  it("walks the command history with Up and Down", async () => {
    render(<ConsolePanel editorId="con-1" sessionId="s-1" />);
    run("PING");
    await waitFor(() => expect(calls("query_run")).toHaveLength(1));
    finishWithArray("PING");
    run("GET a");
    await waitFor(() => expect(calls("query_run")).toHaveLength(2));
    finishWithArray("GET a");
    const input = screen.getByTestId("console-input") as HTMLTextAreaElement;
    fireEvent.keyDown(input, { key: "ArrowUp" });
    expect(input.value).toBe("GET a");
    fireEvent.keyDown(input, { key: "ArrowUp" });
    expect(input.value).toBe("PING");
    fireEvent.keyDown(input, { key: "ArrowDown" });
    expect(input.value).toBe("GET a");
    fireEvent.keyDown(input, { key: "ArrowDown" });
    expect(input.value).toBe("");
  });

  it("asks before a guarded command and runs it with confirmDestructive", async () => {
    refuse = "confirmation required: statement 1: FLUSHDB removes every key of the database";
    render(<ConsolePanel editorId="con-1" sessionId="s-1" />);
    run("FLUSHDB");
    const dialog = await screen.findByRole("dialog", { name: "Run this command?" });
    expect(dialog).toHaveTextContent("FLUSHDB removes every key of the database");
    fireEvent.click(within(dialog).getByTestId("confirm-action"));
    await waitFor(() => expect(calls("query_run")).toHaveLength(2));
    expect(calls("query_run")[1]).toMatchObject({
      script: "FLUSHDB",
      options: { confirmDestructive: true },
    });
  });

  it("words the confirmation reason", () => {
    expect(confirmationText("confirmation required: statement 1: KEYS scans every key")).toBe(
      "This can change or remove data: statement 1: KEYS scans every key.",
    );
  });
});
