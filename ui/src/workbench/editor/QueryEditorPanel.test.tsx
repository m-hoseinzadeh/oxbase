import { act, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { useEffect, useRef } from "react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { CacheStatus, RunEvent, StatementSpan, TreeNode } from "../../api";
import { refreshSessions, resetConnectionsStore } from "../../connections/store";
import type { SessionSummary } from "../../connections/types";
import { tauriCore } from "../../test/tauriMock";
import { QueryEditorPanel } from "./QueryEditorPanel";
import type { EditorInsight, FormatOutcome, SqlEditorProps } from "./SqlEditor";

vi.mock("@tauri-apps/api/core", async () => (await import("../../test/tauriMock")).tauriCore);

/** What the stand-in editor received and how its format command ends. */
const editorMock = vi.hoisted(() => ({
  insight: undefined as EditorInsight | undefined,
  formatOutcome: { status: "formatted" } as FormatOutcome,
  inserted: [] as string[],
}));

// Monaco needs a real browser: a textarea stands in, driven through the same handle.
vi.mock("./SqlEditor", () => ({
  SqlEditor: ({ initialValue, editorRef, insight }: SqlEditorProps) => {
    const area = useRef<HTMLTextAreaElement>(null);
    editorMock.insight = insight;
    useEffect(() => {
      editorRef.current = {
        getText: () => area.current?.value ?? "",
        getSelection: () => {
          const el = area.current;
          if (!el || el.selectionStart === el.selectionEnd) return null;
          return {
            text: el.value.slice(el.selectionStart, el.selectionEnd),
            start: el.selectionStart,
          };
        },
        getCursorOffset: () => area.current?.selectionStart ?? 0,
        setText: (text) => {
          if (area.current) area.current.value = text;
        },
        select: (start, end) => area.current?.setSelectionRange(start, end),
        focus: () => area.current?.focus(),
        format: async () => {
          const outcome = editorMock.formatOutcome;
          editorMock.insight?.onFormat?.(outcome);
          return outcome;
        },
        insertSnippet: (body) => editorMock.inserted.push(body),
      };
      return () => {
        editorRef.current = null;
      };
    }, [editorRef]);
    return <textarea aria-label="SQL editor" ref={area} defaultValue={initialValue} />;
  },
}));

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

function database(name: string, current = false): TreeNode {
  return {
    node: { type: "object", object: { kind: "database", name } },
    key: `object:${name}`,
    label: name,
    expandable: true,
    summary: {
      ref: { kind: "database", name },
      system: false,
      ...(current ? { extras: { current: { type: "bool", value: true } } } : {}),
    },
  };
}

const SCRIPT = "SELECT 1;\nSELECT 2;";

const CACHE: CacheStatus = {
  key: { profileId: "p-1", database: "app" },
  state: "ready",
  source: "disk",
  building: false,
  generation: 7,
  readAt: "2026-11-01T10:00:00Z",
  relationCount: 3,
  columnCount: 12,
  routineCount: 0,
  truncated: false,
  lastBuildMs: 20,
  lastError: null,
};

let onEvent: ((event: RunEvent) => void) | null = null;

function mockCore() {
  tauriCore.invoke.mockImplementation(async (cmd, args) => {
    const a = (args ?? {}) as Record<string, unknown>;
    switch (cmd) {
      case "list_open_connections":
        return [SESSION];
      case "tree_children":
        return [database("app", true), database("other")];
      case "query_run":
        onEvent = (a.onEvent as { onmessage: (e: RunEvent) => void }).onmessage;
        return "run-1";
      case "statement_at": {
        const span: StatementSpan = {
          index: 1,
          startByte: 10,
          endByte: 18,
          startLine: 2,
          startColumn: 1,
          endLine: 2,
          endColumn: 9,
          returnsRows: true,
        };
        return span;
      }
      case "query_cancel":
        return true;
      case "query_close_editor":
        return null;
      case "schema_cache_status":
        return CACHE;
      case "snippet_list":
        return [
          {
            id: "builtin:select",
            name: "Select rows",
            prefix: "sel",
            body: "SELECT $0",
            description: "",
            engines: [],
            builtin: true,
          },
        ];
      default:
        throw new Error(`unexpected command ${cmd}`);
    }
  });
}

function calls(cmd: string): Record<string, unknown>[] {
  return tauriCore.invoke.mock.calls
    .filter(([c]) => c === cmd)
    .map(([, args]) => (args ?? {}) as Record<string, unknown>);
}

async function renderPanel() {
  const view = render(<QueryEditorPanel editorId="editor-1" sql={SCRIPT} />);
  await act(async () => {
    await refreshSessions();
  });
  await screen.findByRole("option", { name: "other" });
  return view;
}

describe("QueryEditorPanel", () => {
  beforeEach(() => {
    resetConnectionsStore();
    tauriCore.invoke.mockReset();
    tauriCore.isTauri.mockReturnValue(true);
    onEvent = null;
    mockCore();
  });
  afterEach(() => {
    resetConnectionsStore();
  });

  it("runs the whole script in the editor's session", async () => {
    await renderPanel();
    expect(screen.getByRole("combobox", { name: "Connection" })).toHaveValue("s-1");
    expect(screen.getByRole("combobox", { name: "Database" })).toHaveValue("app");
    fireEvent.click(screen.getByRole("button", { name: "Run" }));
    await waitFor(() => expect(calls("query_run")).toHaveLength(1));
    const { onEvent: channel, ...args } = calls("query_run")[0] ?? {};
    expect(channel).toBeDefined();
    expect(args).toEqual({
      sessionId: "s-1",
      editorId: "editor-1",
      script: SCRIPT,
      options: null,
    });
  });

  it("runs the statement at the cursor", async () => {
    await renderPanel();
    const editor = screen.getByRole("textbox", { name: "SQL editor" }) as HTMLTextAreaElement;
    editor.setSelectionRange(13, 13);
    fireEvent.click(screen.getByRole("button", { name: "Run current" }));
    await waitFor(() => expect(calls("query_run")).toHaveLength(1));
    expect(calls("statement_at")[0]).toEqual({ engine: "postgres", text: SCRIPT, offset: 13 });
    expect(calls("query_run")[0]?.script).toBe("SELECT 2");
    // The statement that ran is selected in the editor.
    expect(editor.value.slice(editor.selectionStart, editor.selectionEnd)).toBe("SELECT 2");
  });

  it("runs only the selection, in the chosen database", async () => {
    await renderPanel();
    fireEvent.change(screen.getByRole("combobox", { name: "Database" }), {
      target: { value: "other" },
    });
    const editor = screen.getByRole("textbox", { name: "SQL editor" }) as HTMLTextAreaElement;
    editor.setSelectionRange(0, 8);
    fireEvent.click(screen.getByRole("button", { name: "Run selection" }));
    await waitFor(() => expect(calls("query_run")).toHaveLength(1));
    expect(calls("query_run")[0]).toMatchObject({
      script: "SELECT 1",
      options: { database: "other" },
    });
  });

  it("asks for a selection instead of running nothing", async () => {
    await renderPanel();
    fireEvent.click(screen.getByRole("button", { name: "Run selection" }));
    expect(screen.getByRole("status")).toHaveTextContent("Select the SQL to run first.");
    expect(calls("query_run")).toHaveLength(0);
  });

  it("cancels the running query and shows the outcome", async () => {
    await renderPanel();
    const stop = screen.getByRole("button", { name: "Stop" });
    expect(stop).toBeDisabled();
    fireEvent.click(screen.getByRole("button", { name: "Run" }));
    await waitFor(() => expect(onEvent).not.toBeNull());
    act(() => {
      onEvent?.({ type: "started", runId: "run-1", statements: [] });
      onEvent?.({ type: "statementStarted", statementIndex: 0 });
    });
    expect(screen.getByRole("button", { name: "Run" })).toBeDisabled();
    expect(stop).toBeEnabled();
    fireEvent.click(stop);
    await waitFor(() => expect(calls("query_cancel")).toEqual([{ runId: "run-1" }]));
    act(() => onEvent?.({ type: "cancelled", elapsedMs: 50 }));
    expect(screen.getByRole("status")).toHaveTextContent("Cancelled");
    expect(stop).toBeDisabled();
    expect(screen.getByRole("button", { name: "Run" })).toBeEnabled();
  });

  it("shows a start failure as a message", async () => {
    await renderPanel();
    tauriCore.invoke.mockImplementation(async (cmd) => {
      if (cmd === "query_run") throw { category: "invalid_input", message: "editor is busy" };
      return [];
    });
    fireEvent.click(screen.getByRole("button", { name: "Run" }));
    expect(await screen.findByText("editor is busy")).toBeInTheDocument();
    expect(screen.getByRole("status")).toHaveTextContent("Failed");
  });

  it("closes its editor session when the tab closes", async () => {
    const view = await renderPanel();
    fireEvent.click(screen.getByRole("button", { name: "Run" }));
    await waitFor(() => expect(calls("query_run")).toHaveLength(1));
    view.unmount();
    expect(calls("query_close_editor")).toEqual([{ editorId: "editor-1" }]);
  });

  it("waits for the old session's close before running on another connection", async () => {
    const other: SessionSummary = { ...SESSION, id: "s-2", profile_name: "Other PG" };
    let releaseClose: (() => void) | null = null;
    const base = tauriCore.invoke.getMockImplementation();
    tauriCore.invoke.mockImplementation(async (cmd, args) => {
      if (cmd === "list_open_connections") return [SESSION, other];
      if (cmd === "query_close_editor") {
        return new Promise((resolve) => {
          releaseClose = () => resolve(null);
        });
      }
      return base?.(cmd, args);
    });
    await renderPanel();
    fireEvent.click(screen.getByRole("button", { name: "Run" }));
    await waitFor(() => expect(onEvent).not.toBeNull());
    act(() => {
      onEvent?.({ type: "started", runId: "run-1", statements: [] });
      onEvent?.({ type: "finished", elapsedMs: 3 });
    });
    fireEvent.change(screen.getByRole("combobox", { name: "Connection" }), {
      target: { value: "s-2" },
    });
    expect(calls("query_close_editor")).toEqual([{ editorId: "editor-1" }]);
    fireEvent.click(screen.getByRole("button", { name: "Run" }));
    await act(async () => {
      await new Promise((resolve) => setTimeout(resolve, 20));
    });
    // The second run waits for the close.
    expect(calls("query_run")).toHaveLength(1);
    act(() => releaseClose?.());
    await waitFor(() => expect(calls("query_run")).toHaveLength(2));
    expect(calls("query_run")[1]).toMatchObject({ sessionId: "s-2", editorId: "editor-1" });
  });

  it("gives the editor its insight target and shows the schema cache", async () => {
    await renderPanel();
    await waitFor(() =>
      expect(screen.getByTestId("schema-cache-status")).toHaveTextContent("Schema: 3 tables"),
    );
    expect(calls("schema_cache_status").at(-1)).toEqual({ sessionId: "s-1", database: "app" });
    expect(editorMock.insight).toMatchObject({
      editorId: "editor-1",
      engine: "postgres",
      sessionId: "s-1",
      database: "app",
      generation: 7,
    });
  });

  it("formats on demand and reports a failure", async () => {
    await renderPanel();
    editorMock.formatOutcome = { status: "failed", message: "the formatter gave up" };
    fireEvent.click(screen.getByRole("button", { name: "Format" }));
    await waitFor(() =>
      expect(screen.getByRole("status")).toHaveTextContent(
        "Could not format: the formatter gave up",
      ),
    );
    editorMock.formatOutcome = { status: "formatted" };
    fireEvent.click(screen.getByRole("button", { name: "Format" }));
    await waitFor(() => expect(screen.getByRole("status")).not.toHaveTextContent("Could not"));
  });

  it("opens the snippet manager and inserts a snippet", async () => {
    await renderPanel();
    editorMock.inserted = [];
    fireEvent.click(screen.getByRole("button", { name: "Snippets…" }));
    const dialog = await screen.findByRole("dialog", { name: "Snippets" });
    fireEvent.click(await within(dialog).findByText("Select rows"));
    fireEvent.click(within(dialog).getByRole("button", { name: "Insert" }));
    expect(editorMock.inserted).toEqual(["SELECT $0"]);
    expect(screen.queryByRole("dialog")).toBeNull();
  });

  it("needs an open connection", () => {
    tauriCore.invoke.mockResolvedValue([]);
    render(<QueryEditorPanel editorId="editor-2" />);
    expect(screen.getByRole("button", { name: "Run" })).toBeDisabled();
    expect(screen.getByRole("status")).toHaveTextContent("Open a connection to run queries.");
  });
});
